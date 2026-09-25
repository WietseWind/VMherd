# Mock Proxmox API + VNC-over-websocket server for testing VMherd without real VMs.
# Renders what each "VM" receives as text (US layout decoding of QEMU extended key events) and exposes it at /mock/typed.
import asyncio, base64, hashlib, json, os, re, struct, sys, time, urllib.parse
try:  # Pillow draws what the VMs received; without it the screens stay dark (tests do not need it)
    from PIL import Image, ImageDraw, ImageFont
except ImportError:
    Image = None

GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
VMS = {
    9001: dict(vmid=9001, name="lab.mock-1", node="pve1", status="running", type="qemu", template=0, tags="lab;mock", uptime=3600),
    9002: dict(vmid=9002, name="lab.mock-2", node="pve1", status="running", type="qemu", template=0, tags="lab;mock", uptime=7200),
    9003: dict(vmid=9003, name="lab.mock-3", node="pve2", status="stopped", type="qemu", template=0, tags="lab;mock", uptime=0),
    9004: dict(vmid=9004, name="other.web-1", node="pve2", status="running", type="qemu", template=0, tags="web", uptime=99999),
    9005: dict(vmid=9005, name="tpl.ubuntu", node="pve2", status="stopped", type="qemu", template=1, tags="", uptime=0),
}
TICKETS, TASKS, SCREENS, LOG = {}, {}, {}, []
W, H = 720, 400
FONT = None
if Image is not None:
    try:
        FONT = ImageFont.truetype("/System/Library/Fonts/Menlo.ttc", 14)
    except Exception:
        FONT = ImageFont.load_default()

US = {}
for i, c in enumerate("1234567890-="): US[0x02 + i] = (c, "!@#$%^&*()_+"[i])
for i, c in enumerate("qwertyuiop[]"): US[0x10 + i] = (c, "QWERTYUIOP{}"[i])
for i, c in enumerate("asdfghjkl;'`"): US[0x1e + i] = (c, "ASDFGHJKL:\"~"[i])
US[0x2b] = ("\\", "|")
for i, c in enumerate("zxcvbnm,./"): US[0x2c + i] = (c, "ZXCVBNM<>?"[i])
US[0x39] = (" ", " ")
NAMED = {0x1c: "\n", 0x0f: "<Tab>", 0x01: "<Esc>", 0x0e: "<BS>", 0xc8: "<Up>", 0xd0: "<Down>", 0xcb: "<Left>", 0xcd: "<Right>"}


class Screen:
    def __init__(self, vm):
        self.vm, self.text, self.shift, self.ctrl, self.alt, self.clients = vm, "", 0, 0, 0, set()

    def key(self, sc, down):
        if sc in (0x2a, 0x36): self.shift += 1 if down else -1; return
        if sc == 0x1d or sc == 0x9d: self.ctrl += 1 if down else -1; return
        if sc == 0x38 or sc == 0xb8: self.alt += 1 if down else -1; return
        if not down: return
        if sc == 0x0e: self.text = self.text[:-1]
        elif sc in NAMED: self.text += NAMED[sc]
        elif sc in US:
            c = US[sc][1 if self.shift > 0 else 0]
            self.text += f"^{c.upper()}" if self.ctrl > 0 else f"<M-{c}>" if self.alt > 0 else c
        else: self.text += f"<{sc:#x}>"
        for c in self.clients: c.set()

    def render(self):
        if Image is None:
            return None
        im = Image.new("RGB", (W, H), (8, 10, 8))
        d = ImageDraw.Draw(im)
        d.text((8, 6), f"mock VM {self.vm['vmid']} {self.vm['name']} on {self.vm['node']}", fill=(90, 200, 255), font=FONT)
        lines = ("$ " + self.text.replace("\n", "\n$ ")).split("\n")[-22:]
        for i, l in enumerate(lines): d.text((8, 28 + i * 16), l[:95], fill=(120, 255, 120), font=FONT)
        return im


class WS:
    def __init__(self, r, w): self.r, self.w, self.buf = r, w, b""
    async def frame(self):
        b0, b1 = await self.r.readexactly(2)
        n = b1 & 0x7f
        if n == 126: n = struct.unpack(">H", await self.r.readexactly(2))[0]
        elif n == 127: n = struct.unpack(">Q", await self.r.readexactly(8))[0]
        assert b1 & 0x80, "client frame must be masked"
        m = await self.r.readexactly(4)
        p = bytes(x ^ m[i % 4] for i, x in enumerate(await self.r.readexactly(n)))
        return b0 & 0x0f, p
    async def read(self, n):
        while len(self.buf) < n:
            op, p = await self.frame()
            if op == 8: raise EOFError
            if op == 9: self.send(p, 10); continue
            if op in (0, 1, 2): self.buf += p
        out, self.buf = self.buf[:n], self.buf[n:]
        return out
    def send(self, p, op=2):
        n = len(p)
        h = bytes([0x80 | op]) + (bytes([n]) if n < 126 else b"\x7e" + struct.pack(">H", n) if n < 65536 else b"\x7f" + struct.pack(">Q", n))
        self.w.write(h + p)


async def rfb(ws, scr, vmid):
    ws.send(b"RFB 003.008\n")
    ver = await ws.read(12)
    ws.send(b"\x01\x02")               # one security type: VNC auth (like QEMU with a password)
    assert (await ws.read(1)) == b"\x02"
    ws.send(os.urandom(16)); await ws.read(16); ws.send(b"\x00\x00\x00\x00")
    shared = (await ws.read(1))[0]
    LOG.append(f"{vmid} connect ver={ver!r} shared={shared}")
    name = f"mock-{vmid}".encode()
    ws.send(struct.pack(">HHBBBBHHHBBB3xI", W, H, 32, 24, 0, 1, 255, 255, 255, 16, 8, 0, len(name)) + name)
    fmt = (16, 8, 0)
    ev, pending, extkey_sent, want_ext = asyncio.Event(), False, False, False
    ev.set(); scr.clients.add(ev)

    async def sender():
        nonlocal pending, extkey_sent
        while True:
            await ev.wait(); ev.clear()
            while not pending: await asyncio.sleep(0.02)
            pending = False
            rects = []
            if want_ext and not extkey_sent:
                rects.append(struct.pack(">HHHHi", 0, 0, 0, 0, -258)); extkey_sent = True
            im = scr.render()
            if im is None:
                px = bytes(W * H * 4)
            else:
                r, g, b = [im.getchannel(c) for c in "RGB"]
                order = sorted(zip(fmt, (r, g, b)), key=lambda t: t[0])  # little-endian: lowest shift first
                chans = [c for _, c in order] + [Image.new("L", im.size, 0)]
                px = Image.merge("RGBA", chans).tobytes()
            rects.append(struct.pack(">HHHHi", 0, 0, W, H, 0) + px)
            ws.send(struct.pack(">BxH", 0, len(rects)) + b"".join(rects))
            await ws.w.drain()

    st = asyncio.ensure_future(sender())
    try:
        while True:
            t = (await ws.read(1))[0]
            if t == 0:
                d = await ws.read(19); bpp, depth, be, tc, rm, gm, bm, rs, gs, bs = struct.unpack(">3xBBBBHHHBBB3x", d)
                fmt = (rs, gs, bs); LOG.append(f"{vmid} pixfmt {bpp} {be} {fmt}")
            elif t == 2:
                _, n = struct.unpack(">BH", await ws.read(3)); encs = struct.unpack(f">{n}i", await ws.read(4 * n))
                want_ext = -258 in encs
            elif t == 3:
                await ws.read(9); pending = True
            elif t == 4:
                down, ks = struct.unpack(">B2xI", await ws.read(7)); LOG.append(f"{vmid} keysym-only {ks:#x} {down}")
            elif t == 5:
                await ws.read(5)
            elif t == 6:
                n = struct.unpack(">3xI", await ws.read(7))[0]; await ws.read(abs(n))
            elif t == 255:
                sub = (await ws.read(1))[0]
                if sub == 0:
                    down, ks, sc = struct.unpack(">HII", await ws.read(10)); scr.key(sc, down)
                else: raise ValueError(f"qemu sub {sub}")
            elif t == 248:  # fence etc not expected
                raise ValueError("fence")
            else:
                raise ValueError(f"client msg {t}")
    finally:
        st.cancel(); scr.clients.discard(ev); LOG.append(f"{vmid} disconnect")


def reply(w, code, obj, extra=""):
    b = json.dumps(obj).encode()
    w.write(f"HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {len(b)}\r\nConnection: close\r\n{extra}\r\n".encode() + b)


async def handle(r, w):
    try:
        head = (await r.readuntil(b"\r\n\r\n")).decode()
        line, *rest = head.strip().split("\r\n")
        method, target, _ = line.split(" ", 2)
        hdr = {k.lower(): v.strip() for k, v in (l.split(":", 1) for l in rest)}
        body = await r.readexactly(int(hdr.get("content-length", 0)))
        u = urllib.parse.urlsplit(target); q = dict(urllib.parse.parse_qsl(u.query)); form = dict(urllib.parse.parse_qsl(body.decode()))
        p = u.path
        LOG.append(f"HTTP {method} {p} {form or ''}")
        if p == "/mock/typed": return reply(w, 200, {"typed": {k: s.text for k, s in SCREENS.items()}, "log": LOG[-60:]})
        if p == "/api2/json/version": return reply(w, 200, {"data": {"version": "8.4.14", "release": "8.4", "repoid": "mock"}})
        if p == "/api2/json/cluster/resources": return reply(w, 200, {"data": list(VMS.values())})
        m = re.fullmatch(r"/api2/json/nodes/(\w+)/qemu/(\d+)/(vncproxy|vncwebsocket|status/(start|stop|shutdown))", p)
        if m:
            node, vmid, what = m.group(1), int(m.group(2)), m.group(3)
            vm = VMS[vmid]
            if what == "vncproxy":
                if vm["status"] != "running": return reply(w, 500, {"data": None, "message": f"VM {vmid} not running\n"})
                t = f"PVEVNC:MOCK{os.urandom(4).hex()}::sig"; TICKETS[t] = vmid
                return reply(w, 200, {"data": {"port": 5900 + vmid % 100, "ticket": t, "password": "pw" + os.urandom(3).hex(), "user": "mock", "cert": "", "upid": "UPID:x"}})
            if what == "vncwebsocket":
                assert TICKETS.pop(q["vncticket"]) == vmid, "bad ticket"
                assert "binary" in hdr.get("sec-websocket-protocol", ""), "no binary proto"
                acc = base64.b64encode(hashlib.sha1((hdr["sec-websocket-key"] + GUID).encode()).digest()).decode()
                w.write(f"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {acc}\r\nSec-WebSocket-Protocol: binary\r\n\r\n".encode())
                scr = SCREENS.setdefault(vmid, Screen(vm))
                try: await rfb(WS(r, w), scr, vmid)
                except (EOFError, asyncio.IncompleteReadError, ConnectionError): pass
                return
            act = m.group(4)
            await asyncio.sleep(0.5)
            vm["status"] = "running" if act == "start" else "stopped"
            upid = f"UPID:{node}:0000:0000:{int(time.time()):X}:qm{act}:{vmid}:mock:"
            TASKS[upid] = "OK" if vmid != 9004 else "mock failure"
            return reply(w, 200, {"data": upid})
        m = re.fullmatch(r"/api2/json/nodes/(\w+)/tasks/([^/]+)/status", p)
        if m: return reply(w, 200, {"data": {"status": "stopped", "exitstatus": TASKS.get(urllib.parse.unquote(m.group(2)), "?")}})
        reply(w, 404, {"message": "mock: no route " + p})
    except Exception as e:
        LOG.append(f"EXC {e!r}")
    finally:
        try: await w.drain()
        except Exception: pass
        w.close()


async def main():
    s = await asyncio.start_server(handle, "127.0.0.1", 18006)
    print("mock pve on http://127.0.0.1:18006", flush=True)
    async with s: await s.serve_forever()

asyncio.run(main())
