use std::ffi::{CStr, CString};
use std::os::fd::{self, AsRawFd, BorrowedFd};
use std::ptr::{null, null_mut};

use libc::{ftruncate, mmap, shm_open, shm_unlink, O_CREAT, O_EXCL, O_RDWR};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_shm::WlShm;
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::protocol::{wl_display, wl_surface};
use wayland_client::{delegate_noop, EventQueue, Proxy};
use wayland_client::{protocol::wl_registry, Connection, Dispatch, QueueHandle};

use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_manager_v2::ZwpInputMethodManagerV2;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_v2::ZwpInputMethodV2;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2;
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_v2, zwp_input_popup_surface_v2,
};

const NAME: &str = "htrime";

struct Globals {
    input_method_manager: Option<ZwpInputMethodManagerV2>,
    seat: Option<WlSeat>,
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
}

struct State {
    shm: WlShm,
    shm_pool: WlShmPool,
    pointer: WlPointer,
    input_method: ZwpInputMethodV2,
    popup: ZwpInputPopupSurfaceV2,
    surface: WlSurface,
    cairo_surface: cairo::ImageSurface,
}

impl Globals {
    fn new() -> Self {
        Self {
            input_method_manager: None,
            seat: None,
            compositor: None,
            shm: None,
        }
    }
}

fn main() {
    let (mut state, mut wayland_queue) = init();
    loop {
        wayland_queue.blocking_dispatch(&mut state).unwrap();
    }
}

fn shm_file(size: i32) -> BorrowedFd<'static> {
    unsafe {
        let name = CString::new(NAME).unwrap();
        let name_ptr = name.as_ptr();
        let fd = shm_open(name_ptr, O_RDWR | O_CREAT | O_EXCL, 0o600);
        if fd < 0 {
            panic!("shm_open failed")
        }
        shm_unlink(name_ptr);
        loop {
            let ret = ftruncate(fd, size as i64);
            if ret < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                } else {
                    panic!("ftruncate failed")
                }
            } else {
                break;
            }
        }
        BorrowedFd::borrow_raw(fd)
    }
}

fn init() -> (State, EventQueue<State>) {
    let conn = Connection::connect_to_env().unwrap();
    let mut registry_queue: EventQueue<Globals> = conn.new_event_queue();
    let registry_qh = registry_queue.handle();

    let wayland_queue: EventQueue<State> = conn.new_event_queue();
    let wayland_qh = wayland_queue.handle();

    conn.display()
        .get_registry(&registry_qh, wayland_qh.clone());

    let mut state = Globals {
        input_method_manager: None,
        seat: None,
        compositor: None,
        shm: None,
    };
    registry_queue.roundtrip(&mut state).unwrap();

    let compositor = state.compositor.unwrap();
    let manager = state.input_method_manager.unwrap();
    let seat = state.seat.unwrap();

    let shm = state.shm.unwrap();
    let size = 1024 * 1024 * 1024;
    let fd = shm_file(size);
    let shm_pool = shm.create_pool(fd, size, &wayland_qh, ());
    let width = 500;
    let height = 100;
    let stride = width * 4;
    let buffer = shm_pool.create_buffer(
        0,
        width,
        height,
        stride,
        wayland_client::protocol::wl_shm::Format::Argb8888,
        &wayland_qh,
        (),
    );
    let buffer_size = stride * height * width;

    let data = unsafe {
        mmap(
            null_mut::<libc::c_void>(),
            buffer_size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    let data: &mut [u8] =
        unsafe { std::slice::from_raw_parts_mut(data as *mut u8, buffer_size as usize) };

    let pointer = seat.get_pointer(&wayland_qh, ());

    let surface = compositor.create_surface(&wayland_qh, ());

    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, i32::MAX, i32::MAX);
    surface.commit();

    let input_method = manager.get_input_method(&seat, &wayland_qh, ());

    let popup = input_method.get_input_popup_surface(&surface, &wayland_qh, ());

    let cairo_surface =
        cairo::ImageSurface::create_for_data(data, cairo::Format::ARgb32, width, height, stride)
            .unwrap();
    let ctx = cairo::Context::new(&cairo_surface).unwrap();
    ctx.set_source_rgba(1.0, 0.0, 0.0, 1.0);
    ctx.rectangle(0., 0., 100., 100.);
    ctx.fill().unwrap();

    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, i32::MAX, i32::MAX);
    surface.commit();

    (
        State {
            shm,
            pointer,
            input_method,
            surface,
            cairo_surface,
            popup,
            shm_pool,
        },
        wayland_queue,
    )
}

const ZWP_INPUT_METHOD_MANAGER_V2_VERSION: u32 = 1;
const WL_SEAT_VERSION: u32 = 8;

impl Dispatch<wl_registry::WlRegistry, QueueHandle<State>> for Globals {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        handle: &QueueHandle<State>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "zwp_input_method_manager_v2" => {
                    let manager =
                        registry.bind(name, ZWP_INPUT_METHOD_MANAGER_V2_VERSION, handle, ());
                    state.input_method_manager = Some(manager);
                }
                "wl_seat" => {
                    let seat: WlSeat = registry.bind(name, WL_SEAT_VERSION, handle, ());
                    state.seat = Some(seat);
                }
                "wl_compositor" => {
                    let compositor: WlCompositor = registry.bind(name, 4, handle, ());
                    state.compositor = Some(compositor);
                }
                "wl_shm" => {
                    let shm: WlShm = registry.bind(name, 1, handle, ());
                    state.shm = Some(shm);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlCompositor, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &WlCompositor,
        event: <WlCompositor as Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        println!("compositor event");
    }
}
impl Dispatch<WlShmPool, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &WlShmPool,
        event: <WlShmPool as Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        println!("shm pool event");
    }
}
impl Dispatch<WlBuffer, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &WlBuffer,
        event: <WlBuffer as Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        println!("buffer event")
    }
}
impl Dispatch<ZwpInputMethodManagerV2, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwpInputMethodManagerV2,
        event: <ZwpInputMethodManagerV2 as Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        println!("input method manager event");
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &WlSeat,
        event: <WlSeat as Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        println!("seat event");
    }
}

impl Dispatch<WlShm, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &WlShm,
        event: <WlShm as Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        println!("shm event");
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for Globals {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        println!("surface event");
    }
}

impl Dispatch<ZwpInputMethodV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpInputMethodV2,
        event: <ZwpInputMethodV2 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        println!("input method event");
        if let zwp_input_method_v2::Event::Unavailable = event {
            panic!("Input method unavailable.")
        }
    }
}

struct SetPopupDone(bool);

impl Dispatch<ZwpInputPopupSurfaceV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpInputPopupSurfaceV2,
        event: <ZwpInputPopupSurfaceV2 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        println!("popup event");
        if let zwp_input_popup_surface_v2::Event::TextInputRectangle {
            x,
            y,
            width,
            height,
        } = event
        {
            println!("x: {}, y: {}, width: {}, height: {}", x, y, width, height)
        }
    }
}

impl Dispatch<WlSurface, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &WlSurface,
        event: <WlSurface as Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        println!("surface event");
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &WlPointer,
        event: <WlPointer as Proxy>::Event,
        data: &(),
        conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        println!("pointer event");
    }
}
