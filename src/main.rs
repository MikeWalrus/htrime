mod recognition;

use std::ffi::CString;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::raw::c_void;
use std::process::{Child, ChildStdout};
use std::ptr::null_mut;

use libc::{
    epoll_event, epoll_wait, fcntl, ftruncate, mmap, poll, shm_open, EPOLL_CLOEXEC, EPOLL_CTL_ADD,
    F_GETFL, F_SETFL, O_CREAT, O_EXCL, O_NONBLOCK, O_RDWR,
};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_keyboard::{KeyState, KeymapFormat};
use wayland_client::protocol::wl_pointer::{ButtonState, WlPointer};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_shm::WlShm;
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{delegate_noop, event_created_child, EventQueue, Proxy, WEnum};
use wayland_client::{protocol::wl_registry, Connection, Dispatch, QueueHandle};

use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_manager_v2::ZwpTabletManagerV2;
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_v2::ZwpTabletPadV2;
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_seat_v2::{
    self, ZwpTabletSeatV2, EVT_TABLET_ADDED_OPCODE, EVT_TOOL_ADDED_OPCODE,
};
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_tool_v2::{
    self, ZwpTabletToolV2,
};
use wayland_protocols::wp::tablet::zv2::client::zwp_tablet_v2::ZwpTabletV2;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_keyboard_grab_v2::{
    self, ZwpInputMethodKeyboardGrabV2,
};
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_manager_v2::ZwpInputMethodManagerV2;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_v2::ZwpInputMethodV2;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_popup_surface_v2::ZwpInputPopupSurfaceV2;
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_v2, zwp_input_popup_surface_v2,
};

use log::{info, trace, warn};

use xkbcommon::xkb::{Keymap, CONTEXT_NO_FLAGS, KEYMAP_COMPILE_NO_FLAGS, KEYMAP_FORMAT_TEXT_V1};

const NAME: &str = "htrime";

struct Globals {
    input_method_manager: Option<ZwpInputMethodManagerV2>,
    tablet_manager: Option<ZwpTabletManagerV2>,
    seat: Option<WlSeat>,
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
}

struct State {
    conn: Connection,
    wayland_qh: QueueHandle<Self>,
    shm: WlShm,
    shm_pool: WlShmPool,
    pointer: WlPointer,
    input_method: ZwpInputMethodV2,
    input_method_serial: u32,
    popup: ZwpInputPopupSurfaceV2,
    surface: WlSurface,
    cairo_surface: cairo::ImageSurface,
    cairo_ctx: cairo::Context,
    width: i32,
    height: i32,
    original_width: i32,
    original_height: i32,
    strokes: Vec<Stroke>,
    is_pen_down: bool,
    pressure: Option<u32>,
    line_width: f64,
    buffer: WlBuffer,
    data_ptr: *mut c_void,
    xkb_state: Option<XkbState>,
    recognition: Child,
    preedit_text: String,
    max_x: f64,
    max_y: f64,
}

struct XkbState {
    keymap: Keymap,
    xkb_context: xkbcommon::xkb::Context,
    state: xkbcommon::xkb::State,
}

impl Globals {
    fn new() -> Self {
        Self {
            input_method_manager: None,
            seat: None,
            compositor: None,
            shm: None,
            tablet_manager: None,
        }
    }
}

struct InkPoint {
    x: f64,
    y: f64,
    time: u32,
    pressure: Option<u32>,
}

struct Stroke {
    points: Vec<InkPoint>,
}

fn main() {
    env_logger::init();

    let (mut state, mut wayland_queue) = init();

    let epoll_fd = unsafe { libc::epoll_create1(EPOLL_CLOEXEC) };
    assert!(epoll_fd >= 0);

    let (recognition_fd, mut recognition_reader) = epoll_add_recoginition(&mut state, epoll_fd);
    let mut recognition_output = String::new();

    let wayland_fd = epoll_add_wayland(&state, epoll_fd);
    const MAX_EVENTS: usize = 16;
    let mut events = [epoll_event { events: 0, u64: 0 }; MAX_EVENTS];

    loop {
        // flush the outgoing buffers to ensure that the server does receive the messages
        // you've sent

        wayland_queue.flush().unwrap();

        // (this step is only relevant if other threads might be reading the socket as well)
        // make sure you don't have any pending events if the event queue that might have been
        // enqueued by other threads reading the socket
        wayland_queue.dispatch_pending(&mut state).unwrap();

        // This puts in place some internal synchronization to prepare for the fact that
        // you're going to wait for events on the socket and read them, in case other threads
        // are doing the same thing
        let read_guard = wayland_queue.prepare_read().unwrap();

        /*
         * At this point you can invoke epoll(..) to wait for readiness on the multiple FD you
         * are working with, and read_guard.connection_fd() will give you the FD to wait on for
         * the Wayland connection
         */
        let mut wayland_socket_ready = false;
        let mut recognition_ready = false;
        unsafe {
            let num_event = epoll_wait(epoll_fd, events.as_mut_ptr(), MAX_EVENTS as i32, 1000);
            assert!(num_event >= 0);
            if num_event == 0 {
                continue;
            }
            for e in events.iter().take(num_event as usize) {
                if e.u64 == wayland_fd as u64 {
                    wayland_socket_ready = true;
                } else if e.u64 == recognition_fd as u64 {
                    recognition_ready = true;
                }
            }
        }

        if wayland_socket_ready {
            // If epoll notified readiness of the Wayland socket, you can now proceed to the read
            read_guard.read().unwrap();
            // And now, you must invoke dispatch_pending() to actually process the events

            wayland_queue.dispatch_pending(&mut state).unwrap();
        } else {
            // otherwise, some of your other FD are ready, but you didn't receive Wayland events,
            // you can drop the guard to cancel the read preparation
            std::mem::drop(read_guard);
        }

        /*
         * There you process all relevant events from your other event sources
         */
        if recognition_ready {
            unsafe {
                let mut poll_fds = [libc::pollfd {
                    fd: recognition_fd,
                    events: libc::POLLIN,
                    revents: 0,
                }];
                let ret = poll(&mut poll_fds as *mut _, 1, 0);
                if ret == 0 {
                    panic!("poll timeout");
                }
            }
            if recognition_output.ends_with('\n') {
                recognition_output.clear();
            }
            if let Err(e) = recognition_reader.read_line(&mut recognition_output) {
                if e.raw_os_error() == Some(libc::EAGAIN) {
                    warn!("Incomplete line {:?} {e:?}", recognition_output);
                    continue;
                } else {
                    panic!("Failed to read from child: {}", e);
                }
            }
            trace!("recognition output: {}", recognition_output);
            let header = "recognized:";
            if let Some(s) = recognition_output.strip_prefix(header) {
                state.preedit_text.clear();
                state.preedit_text.push_str(s.trim());
                info!("preedit text: {:?}", state.preedit_text);
            }
            state
                .input_method
                .set_preedit_string(state.preedit_text.clone(), 0, 0);
            state.input_method.commit(state.input_method_serial);
        }
    }
}

fn epoll_add_wayland(state: &State, epoll_fd: i32) -> i32 {
    let wayland_fd = state.conn.as_fd().as_raw_fd();
    let mut wayland_event = libc::epoll_event {
        events: libc::EPOLLIN as u32,
        u64: wayland_fd as u64,
    };
    let ret = unsafe { libc::epoll_ctl(epoll_fd, EPOLL_CTL_ADD, wayland_fd, &mut wayland_event) };
    assert!(ret >= 0);
    wayland_fd
}

fn epoll_add_recoginition(state: &mut State, epoll_fd: i32) -> (i32, BufReader<ChildStdout>) {
    let recognition_fd = state.recognition.stdout.as_ref().unwrap().as_raw_fd();
    let mut recognition_event = libc::epoll_event {
        events: libc::EPOLLIN as u32,
        u64: recognition_fd as u64,
    };
    let ret = unsafe {
        libc::epoll_ctl(
            epoll_fd,
            EPOLL_CTL_ADD,
            recognition_fd,
            &mut recognition_event,
        )
    };
    assert!(ret >= 0);
    let handle = state.recognition.stdout.take().unwrap();
    set_nonblocking(&handle, true).unwrap();
    let recognition_reader = BufReader::new(handle);
    (recognition_fd, recognition_reader)
}

fn set_nonblocking<H>(handle: &H, nonblocking: bool) -> std::io::Result<()>
where
    H: Read + AsRawFd,
{
    let fd = handle.as_raw_fd();
    let flags = unsafe { fcntl(fd, F_GETFL, 0) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let flags = if nonblocking {
        flags | O_NONBLOCK
    } else {
        flags & !O_NONBLOCK
    };
    let res = unsafe { fcntl(fd, F_SETFL, flags) };
    if res != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn shm_file(size: i32) -> BorrowedFd<'static> {
    unsafe {
        let name = CString::new(NAME).unwrap();
        let name_ptr = name.as_ptr();
        let fd = shm_open(name_ptr, O_RDWR | O_CREAT | O_EXCL, 0o600);
        if fd < 0 {
            panic!("shm_open failed")
        }
        // shm_unlink(name_ptr);
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

    let mut globals = Globals::new();
    registry_queue.roundtrip(&mut globals).unwrap();

    let compositor = globals.compositor.unwrap();
    let manager = globals.input_method_manager.unwrap();
    let seat = globals.seat.unwrap();
    let tablet_manager = globals.tablet_manager.unwrap();
    tablet_manager.get_tablet_seat(&seat, &wayland_qh, ());

    let shm = globals.shm.unwrap();
    let size = 1024 * 1024 * 1024;
    let fd = shm_file(size);
    let shm_pool = shm.create_pool(fd, size, &wayland_qh, ());
    let width = 200;
    let height = 80;
    let stride = width * 4;
    let buffer_size = stride * height;

    let data_ptr = unsafe {
        mmap(
            null_mut::<libc::c_void>(),
            size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };

    let pointer = seat.get_pointer(&wayland_qh, ());

    let surface = compositor.create_surface(&wayland_qh, ());

    let input_method = manager.get_input_method(&seat, &wayland_qh, ());

    let popup = input_method.get_input_popup_surface(&surface, &wayland_qh, ());

    let _keyboard_grab = input_method.grab_keyboard(&wayland_qh, ());

    let data: &mut [u8] =
        unsafe { std::slice::from_raw_parts_mut(data_ptr as *mut u8, buffer_size as usize) };

    let buffer = shm_pool.create_buffer(
        0,
        width,
        height,
        stride,
        wayland_client::protocol::wl_shm::Format::Argb8888,
        &wayland_qh,
        (),
    );

    let cairo_surface =
        cairo::ImageSurface::create_for_data(data, cairo::Format::ARgb32, width, height, stride)
            .unwrap();
    let ctx = cairo::Context::new(&cairo_surface).unwrap();
    set_line(&ctx);
    fill_background(&ctx);

    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, i32::MAX, i32::MAX);
    surface.commit();

    let recognition = recognition::run();
    let mut state = State {
        shm,
        pointer,
        input_method,
        surface,
        cairo_surface,
        popup,
        shm_pool,
        conn,
        strokes: vec![],
        is_pen_down: false,
        cairo_ctx: ctx,
        buffer,
        data_ptr,
        width,
        height,
        xkb_state: None,
        recognition,
        preedit_text: String::new(),
        input_method_serial: 0,
        pressure: None,
        line_width: 4.,
        wayland_qh,
        max_x: 0.,
        max_y: 0.,
        original_width: width,
        original_height: height,
    };

    (state, wayland_queue)
}

fn fill_background(ctx: &cairo::Context) {
    ctx.save().unwrap();
    ctx.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    ctx.set_operator(cairo::Operator::Source);
    ctx.paint().unwrap();
    ctx.restore().unwrap();
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
                "zwp_tablet_manager_v2" => {
                    let tablet_manager = registry.bind(name, 1, handle, ());
                    state.tablet_manager = Some(tablet_manager);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwpTabletSeatV2, ()> for State {
    event_created_child!(Self, ZwpTabletSeatV2, [
       EVT_TABLET_ADDED_OPCODE => (ZwpTabletV2, ()),
       EVT_TOOL_ADDED_OPCODE => (ZwpTabletToolV2, ()),
    ]);

    fn event(
        _state: &mut Self,
        _proxy: &ZwpTabletSeatV2,
        event: <ZwpTabletSeatV2 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        trace!("tablet seat event");
        match event {
            zwp_tablet_seat_v2::Event::TabletAdded { id: _ } => {}
            zwp_tablet_seat_v2::Event::ToolAdded { id: _ } => {
                info!("tablet tool added");
            }
            zwp_tablet_seat_v2::Event::PadAdded { id: _ } => {}
            _ => {}
        }
    }
}

impl Dispatch<WlCompositor, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: <WlCompositor as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        trace!("compositor event");
    }
}
impl Dispatch<WlShmPool, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlShmPool,
        _event: <WlShmPool as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        trace!("shm pool event");
    }
}
impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlBuffer,
        _event: <WlBuffer as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        trace!("buffer event")
    }
}
impl Dispatch<ZwpInputMethodManagerV2, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpInputMethodManagerV2,
        _event: <ZwpInputMethodManagerV2 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        trace!("input method manager event");
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: <WlSeat as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        trace!("seat event");
    }
}

impl Dispatch<WlShm, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlShm,
        _event: <WlShm as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        trace!("shm event");
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for Globals {
    fn event(
        _state: &mut Self,
        _: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        trace!("surface event");
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
        trace!("input method event");
        match event {
            zwp_input_method_v2::Event::Activate => {
                info!("activate");
            }
            zwp_input_method_v2::Event::Deactivate => {
                info!("deactivate");
            }
            zwp_input_method_v2::Event::Done => {
                state.input_method_serial += 1;
                trace!("done");
            }
            zwp_input_method_v2::Event::Unavailable => {
                panic!("Input method unavailable.")
            }
            _ => {
                trace!("other input method event")
            }
        }
    }
}

impl Dispatch<ZwpInputPopupSurfaceV2, ()> for State {
    fn event(
        _state: &mut Self,
        _: &ZwpInputPopupSurfaceV2,
        event: <ZwpInputPopupSurfaceV2 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        trace!("popup event");
        if let zwp_input_popup_surface_v2::Event::TextInputRectangle {
            x,
            y,
            width,
            height,
        } = event
        {
            trace!("x: {}, y: {}, width: {}, height: {}", x, y, width, height)
        }
    }
}

impl Dispatch<WlSurface, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSurface,
        _event: <WlSurface as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        trace!("surface event");
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlPointer,
        event: <WlPointer as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wayland_client::protocol::wl_pointer::Event::Enter {
                serial: _,
                surface: _,
                surface_x: _,
                surface_y: _,
            } => {
                trace!("enter")
            }
            wayland_client::protocol::wl_pointer::Event::Leave { serial: _, surface } => {
                trace!("leave");
                if state.is_pen_down && surface == state.surface {
                    state.is_pen_down = false;
                    info!("pen up");
                }
            }
            wayland_client::protocol::wl_pointer::Event::Motion {
                time,
                surface_x,
                surface_y,
            } => {
                state.on_motion(surface_x, surface_y, time);
            }
            wayland_client::protocol::wl_pointer::Event::Button {
                serial,
                time,
                button,
                state: button_state,
            } => {
                trace!("button: {serial} {time} {button} {button_state:?}");
                if let WEnum::Value(ButtonState::Pressed) = button_state {
                    state.pressure = None;
                    state.on_down();
                } else {
                    state.pressure = None;
                    state.on_up();
                }
            }
            _ => {
                trace!("other pointer event")
            }
        }
    }
}

impl Dispatch<ZwpTabletManagerV2, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpTabletManagerV2,
        _event: <ZwpTabletManagerV2 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        trace!("tablet manager event");
    }
}

impl Dispatch<ZwpTabletToolV2, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwpTabletToolV2,
        event: <ZwpTabletToolV2 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwp_tablet_tool_v2::Event::Down { serial: _ } => {
                state.on_down();
            }
            zwp_tablet_tool_v2::Event::Up => {
                state.on_up();
            }
            zwp_tablet_tool_v2::Event::Motion { x, y } => {
                state.on_motion(x, y, 0); // TODO: no time available
            }
            zwp_tablet_tool_v2::Event::Pressure { pressure } => {
                trace!("pressure: {}", pressure);
                state.pressure = Some(pressure);
            }
            zwp_tablet_tool_v2::Event::Button {
                serial,
                button,
                state: button_state,
            } => {
                info!("button: {serial} {button} {button_state:?}");
                if let WEnum::Value(zwp_tablet_tool_v2::ButtonState::Pressed) = button_state {
                    match button {
                        331 => {
                            state.enter_input();
                        }
                        332 => {
                            state.undo();
                        }
                        _ => {
                            warn!("unhandled pen button: {}", button)
                        }
                    }
                }
            }
            _ => {
                trace!("other tool event")
            }
        }
    }
}

delegate_noop!(State: ignore ZwpTabletPadV2);
delegate_noop!(State: ignore ZwpTabletV2);

impl Dispatch<ZwpInputMethodKeyboardGrabV2, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwpInputMethodKeyboardGrabV2,
        event: <ZwpInputMethodKeyboardGrabV2 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwp_input_method_keyboard_grab_v2::Event::Keymap { format, fd, size } => unsafe {
                if let WEnum::Value(KeymapFormat::XkbV1) = format {
                    info!("XKB V1 Keymap");
                    let context = xkbcommon::xkb::Context::new(CONTEXT_NO_FLAGS);
                    let keymap = Keymap::new_from_fd(
                        &context,
                        fd,
                        size as usize,
                        KEYMAP_FORMAT_TEXT_V1,
                        KEYMAP_COMPILE_NO_FLAGS,
                    )
                    .unwrap()
                    .unwrap();
                    let xkb_state = xkbcommon::xkb::State::new(&keymap);
                    state.xkb_state = Some(XkbState {
                        keymap,
                        xkb_context: context,
                        state: xkb_state,
                    });
                } else {
                    panic!("Unsupported keymap format")
                }
            },
            zwp_input_method_keyboard_grab_v2::Event::Key {
                serial,
                time,
                key,
                state: key_state,
            } => {
                trace!("key: {serial} {time} {key} {key_state:?}");
                if let WEnum::Value(KeyState::Pressed) = key_state {
                    let c = state
                        .xkb_state
                        .as_ref()
                        .unwrap()
                        .state
                        .key_get_one_sym((key + 8).into());
                    info!("key: {c:?}");
                    if let Some(c) = c.key_char() {
                        match c {
                            'z' => {
                                state.undo();
                            }
                            '\r' => {
                                state.enter_input();
                            }
                            _ => {
                                info!("unhandled key: {c:?}")
                            }
                        }
                    }
                }
            }
            _ => {
                trace!("other keyboard grab event")
            }
        }
    }
}

impl State {
    fn redraw(&mut self) {
        trace!("redraw");
        fill_background(&self.cairo_ctx);
        for stroke in &self.strokes {
            let mut points = stroke.points.iter();
            if let Some(first) = points.next() {
                self.cairo_ctx.move_to(first.x, first.y);
                for point in points {
                    self.set_pressure(point.pressure);
                    trace!(
                        "draw point ({}, {}, {:?})",
                        point.x,
                        point.y,
                        point.pressure
                    );
                    self.cairo_ctx.line_to(point.x, point.y);
                    self.cairo_ctx.stroke().unwrap();
                    self.cairo_ctx.move_to(point.x, point.y);
                }
            }
        }

        self.display();
    }

    fn display(&mut self) {
        self.surface.attach(Some(&self.buffer), 0, 0);
        self.surface.damage(0, 0, i32::MAX, i32::MAX);
        self.surface.commit();
    }

    fn draw_new_point(&mut self, x: f64, y: f64, pressure: Option<u32>) {
        trace!("draw new point ({}, {})", x, y);
        let stroke = self.strokes.last().unwrap();
        if let Some(point) = stroke.points.last() {
            self.set_pressure(pressure);
            self.cairo_ctx.move_to(point.x, point.y);
            self.cairo_ctx.line_to(x, y);
            self.cairo_ctx.stroke().unwrap();
        } else {
            self.cairo_ctx.move_to(x, y);
        }
        self.display()
    }

    fn set_pressure(&self, pressure: Option<u32>) {
        let line_width = if let Some(pressure) = pressure {
            (pressure as f64 / 65535.) * self.line_width
        } else {
            self.line_width
        };
        self.cairo_ctx.set_line_width(line_width);
    }

    fn on_motion(&mut self, surface_x: f64, surface_y: f64, time: u32) {
        trace!("motion: {time} {surface_x}, {surface_y}");
        if self.is_pen_down {
            self.draw_new_point(surface_x, surface_y, self.pressure);
            self.strokes.last_mut().unwrap().points.push(InkPoint {
                x: surface_x,
                y: surface_y,
                time,
                pressure: self.pressure,
            });
            self.max_x = self.max_x.max(surface_x);
            self.max_y = self.max_y.max(surface_y);
            trace!("add point ({surface_x}, {surface_y}) at {time}");
        }
    }

    fn on_down(&mut self) {
        self.is_pen_down = true;
        self.strokes.push(Stroke { points: vec![] });
        info!("pen down, #{}", self.strokes.len());
    }

    fn on_up(&mut self) {
        self.is_pen_down = false;

        self.auto_resize();

        self.recognize();
        info!("pen up");
    }

    fn recognize(&mut self) {
        self.recognition
            .stdin
            .as_ref()
            .unwrap()
            .write_all(format!("{} {}\n", self.width, self.height).as_bytes())
            .unwrap();
        self.recognition.stdin.as_ref().unwrap().flush().unwrap();
    }

    fn resize(&mut self, width: i32, height: i32) {
        self.max_x = 0.;
        self.max_y = 0.;
        self.width = width;
        self.height = height;
        let stride = width * 4;
        let buffer_size = stride * height;
        let data: &mut [u8] = unsafe {
            std::slice::from_raw_parts_mut(self.data_ptr as *mut u8, buffer_size as usize)
        };
        self.buffer.destroy();
        self.buffer = self.shm_pool.create_buffer(
            0,
            width,
            height,
            stride,
            wayland_client::protocol::wl_shm::Format::Argb8888,
            &self.wayland_qh,
            (),
        );
        self.cairo_surface = cairo::ImageSurface::create_for_data(
            data,
            cairo::Format::ARgb32,
            width,
            height,
            stride,
        )
        .unwrap();
        self.cairo_ctx = cairo::Context::new(&self.cairo_surface).unwrap();
        self.redraw();
        self.display();
    }

    fn auto_resize(&mut self) {
        if self.max_x > self.width as f64 * 0.8 {
            info!("auto resize max_x: {}", self.max_x);
            self.resize(self.width + 100, self.height);
        }
    }

    fn restore_size(&mut self) {
        self.resize(self.original_width, self.original_height);
    }

    fn enter_input(&mut self) {
        self.input_method.commit_string(self.preedit_text.clone());
        self.input_method.commit(self.input_method_serial);
        self.strokes.clear();
        self.preedit_text.clear();
        self.restore_size();
        info!("enter input");
    }

    fn undo(&mut self) {
        self.strokes.pop();
        self.redraw();
        self.recognize();
        info!("undo stroke");
    }
}

fn set_line(ctx: &cairo::Context) {
    ctx.set_line_cap(cairo::LineCap::Round);
    ctx.set_line_join(cairo::LineJoin::Round);
    ctx.set_source_rgba(0., 0., 0., 1.);
    ctx.set_line_width(3.);
}
