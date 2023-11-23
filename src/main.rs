use std::ffi::{CStr, CString};
use std::os::fd::{self, AsRawFd, BorrowedFd};
use std::ptr::{null, null_mut};

use libc::{ftruncate, mmap, shm_open, shm_unlink, O_CREAT, O_EXCL, O_RDWR};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_shm::WlShm;
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface;
use wayland_client::{delegate_noop, EventQueue, Proxy};
use wayland_client::{protocol::wl_registry, Connection, Dispatch, QueueHandle};

use wayland_egl::WlEglSurface;
use wayland_protocols::xdg;
use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
use wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_manager_v2::ZwpInputMethodManagerV2;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_v2::ZwpInputMethodV2;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2;
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_v2, zwp_input_popup_surface_v2,
};

const NAME: &str = "htrime";

fn main() {
    init()
}

fn shm_file(size: i32) -> BorrowedFd<'static> {
    unsafe {
        let name = CString::new(NAME).unwrap();
        let name_ptr = name.as_ptr();
        let fd = shm_open(name_ptr, O_RDWR | O_CREAT | O_EXCL, 0600);
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

fn init() {
    let conn = Connection::connect_to_env().unwrap();
    let mut event_queue: EventQueue<State> = conn.new_event_queue();
    let handle = event_queue.handle();
    println!("get registry");

    conn.display().get_registry(&handle, ());

    let mut state = State {
        input_method_manager: None,
        seat: None,
        compositor: None,
        shm: None,
        xdg_wm_base: None,
    };
    event_queue.roundtrip(&mut state).unwrap();

    let compositor = state.compositor.as_ref().unwrap();
    let manager = state.input_method_manager.as_ref().unwrap();
    let seat = state.seat.as_ref().unwrap();

    let xdg_wm_base = state.xdg_wm_base.as_ref().unwrap();

    let shm = state.shm.as_ref().unwrap();
    let size = 1024 * 1024 * 1024;
    let fd = shm_file(size);
    let pool = shm.create_pool(fd, size, &handle, ());
    let width = 500;
    let height = 100;
    let stride = width * 4;
    let buffer = pool.create_buffer(
        0,
        width,
        height,
        stride,
        wayland_client::protocol::wl_shm::Format::Argb8888,
        &handle,
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

    let surface = compositor.create_surface(&handle, ());
    // let xdg_surface = xdg_wm_base.get_xdg_surface(&surface, &handle, ());
    // let toplevel = xdg_surface.get_toplevel(&handle, ());
    // toplevel.set_title("title".to_string());
    // surface.commit();

    // event_queue.roundtrip(&mut state).unwrap();

    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, i32::MAX, i32::MAX);
    surface.commit();


    let mut queue: EventQueue<InputMethodAvailable> = conn.new_event_queue();
    let handle1 = queue.handle();

    let input_method = manager.get_input_method(&seat, &handle1, ());
    let mut input_method_available = InputMethodAvailable(true);
    queue.roundtrip(&mut input_method_available).unwrap();
    if !input_method_available.0 {
        panic!("Input method unavailable.");
    }

    let popup = input_method.get_input_popup_surface(&surface, &handle, ());

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

    loop {
        event_queue.blocking_dispatch(&mut state).unwrap();
    }
}

struct State {
    input_method_manager: Option<ZwpInputMethodManagerV2>,
    seat: Option<WlSeat>,
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    xdg_wm_base: Option<xdg::shell::client::xdg_wm_base::XdgWmBase>,
}

const ZWP_INPUT_METHOD_MANAGER_V2_VERSION: u32 = 1;
const WL_SEAT_VERSION: u32 = 8;

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
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
                    let seat: WlSeat =
                        registry.bind::<WlSeat, (), State>(name, WL_SEAT_VERSION, handle, ());
                    state.seat = Some(seat);
                }
                "wl_compositor" => {
                    let compositor: WlCompositor =
                        registry.bind::<WlCompositor, (), State>(name, 4, handle, ());
                    state.compositor = Some(compositor);
                }
                "wl_shm" => {
                    let shm: WlShm = registry.bind::<WlShm, (), State>(name, 1, handle, ());
                    state.shm = Some(shm);
                }
                "xdg_wm_base" => {
                    let xdg_wm_base: xdg::shell::client::xdg_wm_base::XdgWmBase =
                        registry.bind::<xdg::shell::client::xdg_wm_base::XdgWmBase, (), State>(
                            name,
                            1,
                            handle,
                            (),
                        );
                    state.xdg_wm_base = Some(xdg_wm_base);
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ignore ZwpInputMethodManagerV2);
delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore XdgToplevel);

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        state: &mut Self,
        _: &XdgWmBase,
        event: xdg::shell::client::xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        println!("xdg_wm_base event");
        if let xdg::shell::client::xdg_wm_base::Event::Ping { serial } = event {
            state.xdg_wm_base.as_ref().unwrap().pong(serial);
        }
    }
}

impl Dispatch<xdg::shell::client::xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg::shell::client::xdg_surface::XdgSurface,
        event: xdg::shell::client::xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        println!("xdg_surface event");
        if let xdg::shell::client::xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
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

struct InputMethodAvailable(bool);

impl Dispatch<ZwpInputMethodV2, ()> for InputMethodAvailable {
    fn event(
        state: &mut Self,
        _: &ZwpInputMethodV2,
        event: <ZwpInputMethodV2 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_input_method_v2::Event::Unavailable = event {
            state.0 = false;
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
