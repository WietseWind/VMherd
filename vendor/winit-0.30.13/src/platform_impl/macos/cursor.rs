use std::ffi::c_uchar;
use std::slice;
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::Sel;
use objc2::{msg_send, msg_send_id, sel, ClassType};
use objc2_app_kit::{NSBitmapImageRep, NSCursor, NSDeviceRGBColorSpace, NSImage};
use objc2_foundation::{
    ns_string, NSData, NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSPoint, NSSize,
    NSString,
};

use crate::cursor::{CursorImage, OnlyCursorImageSource};
use crate::window::CursorIcon;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CustomCursor(pub(crate) Retained<NSCursor>);

// SAFETY: NSCursor is immutable and thread-safe
// TODO(madsmtm): Put this logic in objc2-app-kit itself
unsafe impl Send for CustomCursor {}
unsafe impl Sync for CustomCursor {}

impl CustomCursor {
    pub(crate) fn new(cursor: OnlyCursorImageSource) -> CustomCursor {
        Self(cursor_from_image(&cursor.0))
    }
}

pub(crate) fn cursor_from_image(cursor: &CursorImage) -> Retained<NSCursor> {
    let width = cursor.width;
    let height = cursor.height;

    let bitmap = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut::<*mut c_uchar>(),
            width as isize,
            height as isize,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            width as isize * 4,
            32,
        ).unwrap()
    };
    let bitmap_data = unsafe { slice::from_raw_parts_mut(bitmap.bitmapData(), cursor.rgba.len()) };
    bitmap_data.copy_from_slice(&cursor.rgba);

    let image = unsafe {
        NSImage::initWithSize(NSImage::alloc(), NSSize::new(width.into(), height.into()))
    };
    unsafe { image.addRepresentation(&bitmap) };

    let hotspot = NSPoint::new(cursor.hotspot_x as f64, cursor.hotspot_y as f64);

    NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &image, hotspot)
}

pub(crate) fn default_cursor() -> Retained<NSCursor> {
    NSCursor::arrowCursor()
}

// VMherd patch: winit loaded several cursors through undocumented `NSCursor` selectors
// (`_helpCursor`, `_zoomInCursor`, `_windowResize*Cursor`, `busyButClickableCursor`). Those are
// private API, which the Mac App Store rejects. They are replaced by the public cursors that
// macOS 15 added, falling back to the system cursor images that `Move` and `Cell` already use.

/// A public class method on `NSCursor` that newer macOS versions have (`None` on older ones).
unsafe fn try_cursor_from_selector(sel: Sel) -> Option<Retained<NSCursor>> {
    let cls = NSCursor::class();
    if msg_send![cls, respondsToSelector: sel] {
        let cursor: Retained<NSCursor> = unsafe { msg_send_id![cls, performSelector: sel] };
        Some(cursor)
    } else {
        None
    }
}

// `NSCursorFrameResizePosition` and `NSCursorFrameResizeDirectionsAll` (macOS 15).
const FRAME_TOP_LEFT: usize = 1 | 2;
const FRAME_TOP_RIGHT: usize = 1 | 8;
const FRAME_BOTTOM_LEFT: usize = 4 | 2;
const FRAME_BOTTOM_RIGHT: usize = 4 | 8;
const FRAME_DIRECTIONS_ALL: usize = 1 | 2;

/// Diagonal resize cursor: `+[NSCursor frameResizeCursorFromPosition:inDirections:]` on macOS
/// 15 and later, else the system cursor image `name`.
fn frame_resize_cursor(position: usize, name: &str) -> Retained<NSCursor> {
    let cls = NSCursor::class();
    let sel = sel!(frameResizeCursorFromPosition:inDirections:);
    if unsafe { msg_send![cls, respondsToSelector: sel] } {
        unsafe {
            msg_send_id![
                cls,
                frameResizeCursorFromPosition: position,
                inDirections: FRAME_DIRECTIONS_ALL,
            ]
        }
    } else {
        system_cursor(name)
    }
}

const SYSTEM_CURSORS: &str = "/System/Library/Frameworks/ApplicationServices.framework/Versions/\
                              A/Frameworks/HIServices.framework/Versions/A/Resources/cursors";

/// A cursor image shipped with macOS (see `load_webkit_cursor`), or the arrow if this macOS
/// version does not have it.
fn system_cursor(name: &str) -> Retained<NSCursor> {
    if std::path::Path::new(SYSTEM_CURSORS).join(name).join("info.plist").is_file() {
        unsafe { load_webkit_cursor(&NSString::from_str(name)) }
    } else {
        default_cursor()
    }
}

// Note that loading `busybutclickable` with this code won't animate
// the frames; instead you'll just get them all in a column.
unsafe fn load_webkit_cursor(name: &NSString) -> Retained<NSCursor> {
    // Snatch a cursor from WebKit; They fit the style of the native
    // cursors, and will seem completely standard to macOS users.
    //
    // https://stackoverflow.com/a/21786835/5435443
    let root = NSString::from_str(SYSTEM_CURSORS);
    let cursor_path = root.stringByAppendingPathComponent(name);

    let pdf_path = cursor_path.stringByAppendingPathComponent(ns_string!("cursor.pdf"));
    let image = NSImage::initByReferencingFile(NSImage::alloc(), &pdf_path).unwrap();

    // TODO: Handle PLists better
    let info_path = cursor_path.stringByAppendingPathComponent(ns_string!("info.plist"));
    let info: Retained<NSDictionary<NSObject, NSObject>> = unsafe {
        msg_send_id![
            <NSDictionary<NSObject, NSObject>>::class(),
            dictionaryWithContentsOfFile: &*info_path,
        ]
    };
    let mut x = 0.0;
    if let Some(n) = info.get(&*ns_string!("hotx")) {
        if n.is_kind_of::<NSNumber>() {
            let ptr: *const NSObject = n;
            let ptr: *const NSNumber = ptr.cast();
            x = unsafe { &*ptr }.as_cgfloat()
        }
    }
    let mut y = 0.0;
    if let Some(n) = info.get(&*ns_string!("hotx")) {
        if n.is_kind_of::<NSNumber>() {
            let ptr: *const NSObject = n;
            let ptr: *const NSNumber = ptr.cast();
            y = unsafe { &*ptr }.as_cgfloat()
        }
    }

    let hotspot = NSPoint::new(x, y);
    NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &image, hotspot)
}

fn webkit_move() -> Retained<NSCursor> {
    unsafe { load_webkit_cursor(ns_string!("move")) }
}

fn webkit_cell() -> Retained<NSCursor> {
    unsafe { load_webkit_cursor(ns_string!("cell")) }
}

pub(crate) fn invisible_cursor() -> Retained<NSCursor> {
    // 16x16 GIF data for invisible cursor
    // You can reproduce this via ImageMagick.
    // $ convert -size 16x16 xc:none cursor.gif
    static CURSOR_BYTES: &[u8] = &[
        0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x10, 0x00, 0x10, 0x00, 0xf0, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00,
        0x00, 0x00, 0x10, 0x00, 0x10, 0x00, 0x00, 0x02, 0x0e, 0x84, 0x8f, 0xa9, 0xcb, 0xed, 0x0f,
        0xa3, 0x9c, 0xb4, 0xda, 0x8b, 0xb3, 0x3e, 0x05, 0x00, 0x3b,
    ];

    fn new_invisible() -> Retained<NSCursor> {
        // TODO: Consider using `dataWithBytesNoCopy:`
        let data = NSData::with_bytes(CURSOR_BYTES);
        let image = NSImage::initWithData(NSImage::alloc(), &data).unwrap();
        let hotspot = NSPoint::new(0.0, 0.0);
        NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &image, hotspot)
    }

    // Cache this for efficiency
    static CURSOR: OnceLock<CustomCursor> = OnceLock::new();
    CURSOR.get_or_init(|| CustomCursor(new_invisible())).0.clone()
}

pub(crate) fn cursor_from_icon(icon: CursorIcon) -> Retained<NSCursor> {
    match icon {
        CursorIcon::Default => default_cursor(),
        CursorIcon::Pointer => NSCursor::pointingHandCursor(),
        CursorIcon::Grab => NSCursor::openHandCursor(),
        CursorIcon::Grabbing => NSCursor::closedHandCursor(),
        CursorIcon::Text => NSCursor::IBeamCursor(),
        CursorIcon::VerticalText => NSCursor::IBeamCursorForVerticalLayout(),
        CursorIcon::Copy => NSCursor::dragCopyCursor(),
        CursorIcon::Alias => NSCursor::dragLinkCursor(),
        CursorIcon::NotAllowed | CursorIcon::NoDrop => NSCursor::operationNotAllowedCursor(),
        CursorIcon::ContextMenu => NSCursor::contextualMenuCursor(),
        CursorIcon::Crosshair => NSCursor::crosshairCursor(),
        CursorIcon::EResize => NSCursor::resizeRightCursor(),
        CursorIcon::NResize => NSCursor::resizeUpCursor(),
        CursorIcon::WResize => NSCursor::resizeLeftCursor(),
        CursorIcon::SResize => NSCursor::resizeDownCursor(),
        CursorIcon::EwResize | CursorIcon::ColResize => NSCursor::resizeLeftRightCursor(),
        CursorIcon::NsResize | CursorIcon::RowResize => NSCursor::resizeUpDownCursor(),
        CursorIcon::Help => system_cursor("help"),
        CursorIcon::ZoomIn => unsafe { try_cursor_from_selector(sel!(zoomInCursor)) }
            .unwrap_or_else(|| system_cursor("zoomin")),
        CursorIcon::ZoomOut => unsafe { try_cursor_from_selector(sel!(zoomOutCursor)) }
            .unwrap_or_else(|| system_cursor("zoomout")),
        CursorIcon::NeResize => frame_resize_cursor(FRAME_TOP_RIGHT, "resizenortheast"),
        CursorIcon::NwResize => frame_resize_cursor(FRAME_TOP_LEFT, "resizenorthwest"),
        CursorIcon::SeResize => frame_resize_cursor(FRAME_BOTTOM_RIGHT, "resizesoutheast"),
        CursorIcon::SwResize => frame_resize_cursor(FRAME_BOTTOM_LEFT, "resizesouthwest"),
        CursorIcon::NeswResize => frame_resize_cursor(FRAME_TOP_RIGHT, "resizenortheastsouthwest"),
        CursorIcon::NwseResize => frame_resize_cursor(FRAME_TOP_LEFT, "resizenorthwestsoutheast"),
        // There is no public busy cursor (the system shows the beach ball when the app hangs),
        // and the `busybutclickable` image does not animate.
        CursorIcon::Wait | CursorIcon::Progress => default_cursor(),
        CursorIcon::Move | CursorIcon::AllScroll => webkit_move(),
        CursorIcon::Cell => webkit_cell(),
        _ => default_cursor(),
    }
}
