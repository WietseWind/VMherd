//! API calls and error mapping against a local plain-HTTP mock.

mod common;

use pve::{Client, Error, PowerAction, VmKind, VmRef, VncProxy};
use tokio::net::TcpListener;

use common::{Reply, Request, endpoint, expected_auth, serve};

async fn client_for<F>(handler: F) -> (Client, common::Log)
where
    F: Fn(&Request) -> Reply + Send + Sync + 'static,
{
    let (addr, log) = serve(None, handler).await;
    (Client::new(&endpoint(&format!("http://{addr}"), None)).unwrap(), log)
}

fn qemu() -> VmRef {
    VmRef { vmid: 101, node: "pve2".into(), kind: VmKind::Qemu }
}

fn api_error(result: Result<impl std::fmt::Debug, Error>) -> (u16, String) {
    match result {
        Err(Error::Api { status, message }) => (status, message),
        other => panic!("expected an API error, got {other:?}"),
    }
}

#[tokio::test]
async fn requests_carry_token_and_user_agent() {
    let (client, log) = client_for(|_| Reply::data(r#"{"version":"8.4.14"}"#)).await;
    assert_eq!(client.version().await.unwrap().version, "8.4.14");
    let requests = log.lock().unwrap();
    let req = &requests[0];
    assert_eq!((req.method.as_str(), req.target.as_str()), ("GET", "/api2/json/version"));
    assert_eq!(req.header("authorization"), Some(expected_auth().as_str()));
    assert!(req.header("user-agent").unwrap().starts_with("vmherd/"));
}

#[tokio::test]
async fn unauthorized_uses_the_reason_phrase() {
    let (client, _) =
        client_for(|_| Reply { status: 401, reason: "invalid token value!".into(), body: String::new() }).await;
    assert_eq!(api_error(client.version().await), (401, "invalid token value!".to_owned()));
}

#[tokio::test]
async fn server_error_uses_the_body_message() {
    let (client, _) = client_for(|_| {
        Reply::json(
            500,
            r#"{"data":null,"message":"Configuration file 'nodes/pve2/qemu-server/101.conf' does not exist\n"}"#,
        )
    })
    .await;
    let (status, message) = api_error(client.power(&qemu(), PowerAction::Start).await);
    assert_eq!(status, 500);
    assert_eq!(message, "Configuration file 'nodes/pve2/qemu-server/101.conf' does not exist");
}

#[tokio::test]
async fn bad_request_lists_the_parameter_errors() {
    let (client, _) = client_for(|_| {
        Reply::json(
            400,
            r#"{"errors":{"type":"value 'vm' is odd"},"message":"Parameter verification failed.\n","data":null}"#,
        )
    })
    .await;
    let (status, message) = api_error(client.vms().await);
    assert_eq!(status, 400);
    assert_eq!(message, "Parameter verification failed. (type: value 'vm' is odd)");
}

#[tokio::test]
async fn unexpected_json_is_a_decode_error() {
    let (client, _) = client_for(|_| Reply::json(200, r#"{"nodata":1}"#)).await;
    assert!(matches!(client.version().await, Err(Error::Decode(_))));
}

#[tokio::test]
async fn unreachable_host_is_a_network_error() {
    let addr = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let client = Client::new(&endpoint(&format!("http://{addr}"), None)).unwrap();
    match client.version().await {
        Err(Error::Network { host, detail }) => {
            assert_eq!(host, addr.to_string());
            assert!(!detail.is_empty());
            assert!(!detail.contains(common::TOKEN_SECRET));
        }
        other => panic!("expected a network error, got {other:?}"),
    }
}

#[tokio::test]
async fn vm_list() {
    let (client, log) = client_for(|_| {
        Reply::data(
            r#"[{"vmid":101,"name":"web","node":"pve2","status":"running","type":"qemu","template":0,"tags":"a;b","uptime":5,"maxmem":1},
                {"vmid":200,"node":"pve3","status":"stopped","type":"lxc","template":1,"lock":"backup"}]"#,
        )
    })
    .await;
    let vms = client.vms().await.unwrap();
    assert_eq!(vms.len(), 2);
    assert_eq!(vms[0].vm_ref(), qemu());
    assert!(vms[1].template);
    assert_eq!(vms[1].lock.as_deref(), Some("backup"));
    assert_eq!(log.lock().unwrap()[0].target, "/api2/json/cluster/resources?type=vm");
}

#[tokio::test]
async fn vnc_proxy_qemu_generates_a_password() {
    let (client, log) = client_for(|_| {
        Reply::data(
            r#"{"port":5900,"ticket":"PVEVNC:T::s","password":"Ab3dEf7h","upid":"UPID:x","cert":"c","user":"u"}"#,
        )
    })
    .await;
    let proxy = client.vnc_proxy(&qemu()).await.unwrap();
    assert_eq!(proxy, VncProxy { port: 5900, ticket: "PVEVNC:T::s".into(), password: "Ab3dEf7h".into() });
    let requests = log.lock().unwrap();
    assert_eq!((requests[0].method.as_str(), requests[0].path()), ("POST", "/api2/json/nodes/pve2/qemu/101/vncproxy"));
    assert_eq!(requests[0].body, "websocket=1&generate-password=1");
}

#[tokio::test]
async fn vnc_proxy_retries_without_generate_password() {
    let (client, log) = client_for(|req| {
        if req.body.contains("generate-password") {
            Reply::json(
                400,
                r#"{"errors":{"generate-password":"property is not defined in schema and the schema does not allow additional properties"},"message":"Parameter verification failed.\n","data":null}"#,
            )
        } else {
            Reply::data(r#"{"port":"5901","ticket":"PVEVNC:T::s","upid":"UPID:x"}"#)
        }
    })
    .await;
    let proxy = client.vnc_proxy(&qemu()).await.unwrap();
    assert_eq!((proxy.port, proxy.password.as_str()), (5901, "PVEVNC:T::s"));
    let bodies: Vec<_> = log.lock().unwrap().iter().map(|r| r.body.clone()).collect();
    assert_eq!(bodies, ["websocket=1&generate-password=1", "websocket=1"]);
}

#[tokio::test]
async fn vnc_proxy_other_400_is_not_retried() {
    let (client, log) = client_for(|_| {
        Reply::json(400, r#"{"errors":{"websocket":"bad"},"message":"Parameter verification failed.\n"}"#)
    })
    .await;
    assert_eq!(api_error(client.vnc_proxy(&qemu()).await).0, 400);
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn vnc_proxy_lxc_uses_the_ticket_as_password() {
    let (client, log) = client_for(|_| Reply::data(r#"{"port":5900,"ticket":"PVEVNC:L::s"}"#)).await;
    let lxc = VmRef { vmid: 200, node: "pve3".into(), kind: VmKind::Lxc };
    let proxy = client.vnc_proxy(&lxc).await.unwrap();
    assert_eq!(proxy.password, "PVEVNC:L::s");
    let requests = log.lock().unwrap();
    assert_eq!(requests[0].path(), "/api2/json/nodes/pve3/lxc/200/vncproxy");
    assert_eq!(requests[0].body, "websocket=1");
}

#[tokio::test]
async fn power_posts_the_action() {
    let upid = "UPID:pve2:0001:0002:6720AB00:qmshutdown:101:root@pam!grid-test:";
    let (client, log) = client_for(move |_| Reply::data(&format!("\"{upid}\""))).await;
    assert_eq!(client.power(&qemu(), PowerAction::Shutdown).await.unwrap(), upid);
    let requests = log.lock().unwrap();
    assert_eq!(
        (requests[0].method.as_str(), requests[0].path()),
        ("POST", "/api2/json/nodes/pve2/qemu/101/status/shutdown")
    );
    assert_eq!(requests[0].body, "");
}

#[tokio::test]
async fn task_status_path_and_parsing() {
    let (client, log) = client_for(|_| Reply::data(r#"{"status":"stopped","exitstatus":"OK","upid":"x"}"#)).await;
    let status = client.task_status("pve2", "UPID:pve2:0001:0002:6720AB00:qmstart:101:root@pam!t:").await.unwrap();
    assert!(status.is_done() && status.is_ok());
    assert_eq!(
        log.lock().unwrap()[0].path(),
        "/api2/json/nodes/pve2/tasks/UPID:pve2:0001:0002:6720AB00:qmstart:101:root@pam!t:/status"
    );
}

#[tokio::test]
async fn invalid_node_is_rejected_locally() {
    let (client, log) = client_for(|_| Reply::data("null")).await;
    let vm = VmRef { vmid: 1, node: "../../access".into(), kind: VmKind::Qemu };
    assert!(matches!(client.vnc_proxy(&vm).await, Err(Error::Config(_))));
    assert!(matches!(client.power(&vm, PowerAction::Stop).await, Err(Error::Config(_))));
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn websocket_upgrade_refused_is_an_api_error() {
    let (client, log) =
        client_for(|_| Reply::json(401, r#"{"data":null,"message":"permission denied - invalid PVE ticket\n"}"#)).await;
    let proxy = VncProxy { port: 5900, ticket: "PVEVNC:T::s".into(), password: "x".into() };
    let (status, message) = api_error(client.vnc_connect(&qemu(), &proxy).await);
    assert_eq!(status, 401);
    // tungstenite only has the body if it arrived together with the head.
    assert!(message == "permission denied - invalid PVE ticket" || message == "Unauthorized", "{message}");
    let requests = log.lock().unwrap();
    assert_eq!(requests[0].path(), "/api2/json/nodes/pve2/qemu/101/vncwebsocket");
    assert_eq!(requests[0].header("upgrade"), Some("websocket"));
}

/// Set in the child process of `requests_ignore_proxy_variables`.
const PROXY_CHILD: &str = "PVE_TEST_PROXY_CHILD";

/// Proxy variables must not divert REST calls: they go straight to the cluster, like the
/// console websocket. The variables are set for a child process (this test binary again),
/// because changing this multi-threaded process's environment would need `unsafe`.
#[tokio::test]
async fn requests_ignore_proxy_variables() {
    if std::env::var_os(PROXY_CHILD).is_some() {
        // In the child every proxy variable points at a closed port.
        let (client, log) = client_for(|_| Reply::data(r#"{"version":"8.4.14"}"#)).await;
        assert_eq!(client.version().await.unwrap().version, "8.4.14");
        assert_eq!(log.lock().unwrap().len(), 1);
        return;
    }
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let proxy = format!("http://{closed}");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child.args(["requests_ignore_proxy_variables", "--exact", "--test-threads=1"]).env(PROXY_CHILD, "1");
    for var in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        child.env(var, &proxy);
    }
    for var in ["NO_PROXY", "no_proxy", "REQUEST_METHOD"] {
        child.env_remove(var);
    }
    let output = tokio::task::spawn_blocking(move || child.output()).await.unwrap().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "child run failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
