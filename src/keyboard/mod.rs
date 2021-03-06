//! Utilities for keymap interpretation of keyboard input
//!
//! This module provides an implementation for `wl_keyboard`
//! objects using `libxkbcommon` to interpret the keyboard input
//! given the user keymap.
//!
//! You simply need to provide an implementation to receive the
//! intepreted events, as described by the `Event` enum of this modules.
//!
//! Implementation of you `NewProxy<WlKeyboard>` can be done with the
//! `map_keyboard_auto` or the `map_keyboard_rmlvo` functions depending
//! on wether you wish to use the keymap provided by the server or a
//! specific one.

use std::env;
use std::ffi::CString;
use std::fs::File;
use std::os::raw::c_char;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::io::{FromRawFd, RawFd};
use std::ptr;

use memmap::MmapOptions;

use wayland_client::commons::Implementation;
pub use wayland_client::protocol::wl_keyboard::KeyState;
use wayland_client::protocol::{wl_keyboard, wl_surface};
use wayland_client::{NewProxy, Proxy};

use self::ffi::xkb_state_component;
use self::ffi::XKBCOMMON_HANDLE as XKBH;

mod ffi;
pub mod keysyms;

struct KbState {
    xkb_context: *mut ffi::xkb_context,
    xkb_keymap: *mut ffi::xkb_keymap,
    xkb_state: *mut ffi::xkb_state,
    xkb_compose_table: *mut ffi::xkb_compose_table,
    xkb_compose_state: *mut ffi::xkb_compose_state,
    mods_state: ModifiersState,
    locked: bool,
}

/// Represents the current state of the keyboard modifiers
///
/// Each field of this struct represents a modifier and is `true` if this modifier is active.
///
/// For some modifiers, this means that the key is currently pressed, others are toggled
/// (like caps lock).
#[derive(Copy, Clone, Debug)]
pub struct ModifiersState {
    /// The "control" key
    pub ctrl: bool,
    /// The "alt" key
    pub alt: bool,
    /// The "shift" key
    pub shift: bool,
    /// The "Caps lock" key
    pub caps_lock: bool,
    /// The "logo" key
    ///
    /// Also known as the "windows" key on most keyboards
    pub logo: bool,
    /// The "Num lock" key
    pub num_lock: bool,
}

impl ModifiersState {
    fn new() -> ModifiersState {
        ModifiersState {
            ctrl: false,
            alt: false,
            shift: false,
            caps_lock: false,
            logo: false,
            num_lock: false,
        }
    }

    fn update_with(&mut self, state: *mut ffi::xkb_state) {
        self.ctrl = unsafe {
            (XKBH.xkb_state_mod_name_is_active)(
                state,
                ffi::XKB_MOD_NAME_CTRL.as_ptr() as *const c_char,
                xkb_state_component::XKB_STATE_MODS_EFFECTIVE,
            ) > 0
        };
        self.alt = unsafe {
            (XKBH.xkb_state_mod_name_is_active)(
                state,
                ffi::XKB_MOD_NAME_ALT.as_ptr() as *const c_char,
                xkb_state_component::XKB_STATE_MODS_EFFECTIVE,
            ) > 0
        };
        self.shift = unsafe {
            (XKBH.xkb_state_mod_name_is_active)(
                state,
                ffi::XKB_MOD_NAME_SHIFT.as_ptr() as *const c_char,
                xkb_state_component::XKB_STATE_MODS_EFFECTIVE,
            ) > 0
        };
        self.caps_lock = unsafe {
            (XKBH.xkb_state_mod_name_is_active)(
                state,
                ffi::XKB_MOD_NAME_CAPS.as_ptr() as *const c_char,
                xkb_state_component::XKB_STATE_MODS_EFFECTIVE,
            ) > 0
        };
        self.logo = unsafe {
            (XKBH.xkb_state_mod_name_is_active)(
                state,
                ffi::XKB_MOD_NAME_LOGO.as_ptr() as *const c_char,
                xkb_state_component::XKB_STATE_MODS_EFFECTIVE,
            ) > 0
        };
        self.num_lock = unsafe {
            (XKBH.xkb_state_mod_name_is_active)(
                state,
                ffi::XKB_MOD_NAME_NUM.as_ptr() as *const c_char,
                xkb_state_component::XKB_STATE_MODS_EFFECTIVE,
            ) > 0
        };
    }
}

unsafe impl Send for KbState {}

impl KbState {
    fn update_modifiers(
        &mut self,
        mods_depressed: u32,
        mods_latched: u32,
        mods_locked: u32,
        group: u32,
    ) {
        if !self.ready() {
            return;
        }
        let mask = unsafe {
            (XKBH.xkb_state_update_mask)(
                self.xkb_state,
                mods_depressed,
                mods_latched,
                mods_locked,
                0,
                0,
                group,
            )
        };
        if mask.contains(xkb_state_component::XKB_STATE_MODS_EFFECTIVE) {
            // effective value of mods have changed, we need to update our state
            self.mods_state.update_with(self.xkb_state);
        }
    }

    fn get_one_sym_raw(&mut self, keycode: u32) -> u32 {
        if !self.ready() {
            return 0;
        }
        unsafe { (XKBH.xkb_state_key_get_one_sym)(self.xkb_state, keycode + 8) }
    }

    fn get_utf8_raw(&mut self, keycode: u32) -> Option<String> {
        if !self.ready() {
            return None;
        }
        let size = unsafe {
            (XKBH.xkb_state_key_get_utf8)(self.xkb_state, keycode + 8, ptr::null_mut(), 0)
        } + 1;
        if size <= 1 {
            return None;
        };
        let mut buffer = Vec::with_capacity(size as usize);
        unsafe {
            buffer.set_len(size as usize);
            (XKBH.xkb_state_key_get_utf8)(
                self.xkb_state,
                keycode + 8,
                buffer.as_mut_ptr() as *mut _,
                size as usize,
            );
        };
        // remove the final `\0`
        buffer.pop();
        // libxkbcommon will always provide valid UTF8
        Some(unsafe { String::from_utf8_unchecked(buffer) })
    }

    fn compose_feed(&mut self, keysym: u32) -> Option<ffi::xkb_compose_feed_result> {
        if !self.ready() || self.xkb_compose_state.is_null() {
            return None;
        }
        Some(unsafe { (XKBH.xkb_compose_state_feed)(self.xkb_compose_state, keysym) })
    }

    fn compose_status(&mut self) -> Option<ffi::xkb_compose_status> {
        if !self.ready() || self.xkb_compose_state.is_null() {
            return None;
        }
        Some(unsafe { (XKBH.xkb_compose_state_get_status)(self.xkb_compose_state) })
    }

    fn compose_get_utf8(&mut self) -> Option<String> {
        if !self.ready() || self.xkb_compose_state.is_null() {
            return None;
        }
        let size = unsafe {
            (XKBH.xkb_compose_state_get_utf8)(self.xkb_compose_state, ptr::null_mut(), 0)
        } + 1;
        if size <= 1 {
            return None;
        };
        let mut buffer = Vec::with_capacity(size as usize);
        unsafe {
            buffer.set_len(size as usize);
            (XKBH.xkb_compose_state_get_utf8)(
                self.xkb_compose_state,
                buffer.as_mut_ptr() as *mut _,
                size as usize,
            );
        };
        // remove the final `\0`
        buffer.pop();
        // libxkbcommon will always provide valid UTF8
        Some(unsafe { String::from_utf8_unchecked(buffer) })
    }

    fn new() -> Result<KbState, Error> {
        let xkbh = match ffi::XKBCOMMON_OPTION.as_ref() {
            Some(h) => h,
            None => return Err(Error::XKBNotFound),
        };
        let xkb_context =
            unsafe { (xkbh.xkb_context_new)(ffi::xkb_context_flags::XKB_CONTEXT_NO_FLAGS) };
        if xkb_context.is_null() {
            return Err(Error::XKBNotFound);
        }

        let mut me = KbState {
            xkb_context: xkb_context,
            xkb_keymap: ptr::null_mut(),
            xkb_state: ptr::null_mut(),
            xkb_compose_table: ptr::null_mut(),
            xkb_compose_state: ptr::null_mut(),
            mods_state: ModifiersState::new(),
            locked: false,
        };

        unsafe {
            me.init_compose();
        }

        Ok(me)
    }

    unsafe fn init_compose(&mut self) {
        let locale = env::var_os("LC_ALL")
            .or_else(|| env::var_os("LC_CTYPE"))
            .or_else(|| env::var_os("LANG"))
            .unwrap_or_else(|| "C".into());
        let locale = CString::new(locale.into_vec()).unwrap();

        let compose_table = (XKBH.xkb_compose_table_new_from_locale)(
            self.xkb_context,
            locale.as_ptr(),
            ffi::xkb_compose_compile_flags::XKB_COMPOSE_COMPILE_NO_FLAGS,
        );

        if compose_table.is_null() {
            // init of compose table failed, continue without compose
            return;
        }

        let compose_state = (XKBH.xkb_compose_state_new)(
            compose_table,
            ffi::xkb_compose_state_flags::XKB_COMPOSE_STATE_NO_FLAGS,
        );

        if compose_state.is_null() {
            // init of compose state failed, continue without compose
            (XKBH.xkb_compose_table_unref)(compose_table);
            return;
        }

        self.xkb_compose_table = compose_table;
        self.xkb_compose_state = compose_state;
    }

    unsafe fn post_init(&mut self, xkb_keymap: *mut ffi::xkb_keymap) {
        let xkb_state = (XKBH.xkb_state_new)(xkb_keymap);
        self.xkb_keymap = xkb_keymap;
        self.xkb_state = xkb_state;
        self.mods_state.update_with(xkb_state);
    }

    unsafe fn de_init(&mut self) {
        (XKBH.xkb_state_unref)(self.xkb_state);
        self.xkb_state = ptr::null_mut();
        (XKBH.xkb_keymap_unref)(self.xkb_keymap);
        self.xkb_keymap = ptr::null_mut();
    }

    unsafe fn init_with_fd(&mut self, fd: RawFd, size: usize) {
        let map = MmapOptions::new()
            .len(size)
            .map(&File::from_raw_fd(fd))
            .unwrap();

        let xkb_keymap = (XKBH.xkb_keymap_new_from_string)(
            self.xkb_context,
            map.as_ptr() as *const _,
            ffi::xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
            ffi::xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
        );

        if xkb_keymap.is_null() {
            panic!("Received invalid keymap from compositor.");
        }

        self.post_init(xkb_keymap);
    }

    unsafe fn init_with_rmlvo(&mut self, names: ffi::xkb_rule_names) -> Result<(), Error> {
        let xkb_keymap = (XKBH.xkb_keymap_new_from_names)(
            self.xkb_context,
            &names,
            ffi::xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
        );

        if xkb_keymap.is_null() {
            return Err(Error::BadNames);
        }

        self.post_init(xkb_keymap);

        Ok(())
    }

    #[inline]
    fn ready(&self) -> bool {
        !self.xkb_state.is_null()
    }
}

impl Drop for KbState {
    fn drop(&mut self) {
        unsafe {
            (XKBH.xkb_compose_state_unref)(self.xkb_compose_state);
            (XKBH.xkb_compose_table_unref)(self.xkb_compose_table);
            (XKBH.xkb_state_unref)(self.xkb_state);
            (XKBH.xkb_keymap_unref)(self.xkb_keymap);
            (XKBH.xkb_context_unref)(self.xkb_context);
        }
    }
}

#[derive(Debug)]
/// An error that occured while trying to initialize a mapped keyboard
pub enum Error {
    /// libxkbcommon is not available
    XKBNotFound,
    /// Provided RMLVO sepcified a keymap that would not be loaded
    BadNames,
}

/// The RMLVO description of a keymap
///
/// All fiels are optional, and the system default
/// will be used if set to `None`.
pub struct RMLVO {
    /// The rules file to use
    pub rules: Option<String>,
    /// The keyboard model by which to interpret keycodes and LEDs
    pub model: Option<String>,
    /// A comma seperated list of layouts (languages) to include in the keymap
    pub layout: Option<String>,
    /// A comma seperated list of variants, one per layout, which may modify or
    /// augment the respective layout in various ways
    pub variant: Option<String>,
    /// A comma seprated list of options, through which the user specifies
    /// non-layout related preferences, like which key combinations are
    /// used for switching layouts, or which key is the Compose key.
    pub options: Option<String>,
}

/// Events received from a mapped keyboard
pub enum Event<'a> {
    /// The keyboard focus has entered a surface
    Enter {
        /// serial number of the event
        serial: u32,
        /// surface that was entered
        surface: Proxy<wl_surface::WlSurface>,
        /// current state of the modifiers
        modifiers: ModifiersState,
        /// raw values of the currently pressed keys
        rawkeys: &'a [u32],
        /// interpreted symbols of the currently pressed keys
        keysyms: &'a [u32],
    },
    /// The keyboard focus has left a surface
    Leave {
        /// serial number of the event
        serial: u32,
        /// surface that was left
        surface: Proxy<wl_surface::WlSurface>,
    },
    /// A key event occured
    Key {
        /// serial number of the event
        serial: u32,
        /// time at which the keypress occured
        time: u32,
        /// current state of the modifiers
        modifiers: ModifiersState,
        /// raw value of the key
        rawkey: u32,
        /// interpreted symbol of the key
        keysym: u32,
        /// new state of the key
        state: KeyState,
        /// utf8 interpretation of the entered text
        ///
        /// will always be `None` on key release events
        utf8: Option<String>,
    },
    /// Repetition information advertizing
    RepeatInfo {
        /// rate (in milisecond) at which the repetition should occur
        rate: i32,
        /// delay (in milisecond) between a key press and the start of repetition
        delay: i32,
    },
}

/// Implement a keyboard to automatically detect the keymap
///
/// This requires you to provide an implementation to receive the events after they
/// have been interpreted with the keymap.
///
/// The keymap information will be loaded from the events sent by the compositor,
/// as such you need to call this method as soon as you have created the keyboard
/// to make sure this event does not get lost.
///
/// Returns an error if xkbcommon could not be initialized.
pub fn map_keyboard_auto<Impl>(
    keyboard: NewProxy<wl_keyboard::WlKeyboard>,
    implementation: Impl,
) -> Result<Proxy<wl_keyboard::WlKeyboard>, (Error, NewProxy<wl_keyboard::WlKeyboard>)>
where
    for<'a> Impl: Implementation<Proxy<wl_keyboard::WlKeyboard>, Event<'a>> + Send,
{
    let state = match KbState::new() {
        Ok(s) => s,
        Err(e) => return Err((e, keyboard)),
    };
    Ok(implement_kbd(keyboard, state, implementation))
}

/// Implement a keyboard for a predefined keymap
///
/// This requires you to provide an implementation to receive the events after they
/// have been interpreted with the keymap.
///
/// The keymap will be loaded from the provided RMLVO rules. Any keymap provided
/// by the compositor will be ignored.
///
/// Returns an error if xkbcommon could not be initialized or the RMLVO specification
/// contained invalid values.
pub fn map_keyboard_rmlvo<Impl>(
    keyboard: NewProxy<wl_keyboard::WlKeyboard>,
    rmlvo: RMLVO,
    implementation: Impl,
) -> Result<Proxy<wl_keyboard::WlKeyboard>, (Error, NewProxy<wl_keyboard::WlKeyboard>)>
where
    for<'a> Impl: Implementation<Proxy<wl_keyboard::WlKeyboard>, Event<'a>> + Send,
{
    fn to_cstring(s: Option<String>) -> Result<Option<CString>, Error> {
        s.map_or(Ok(None), |s| CString::new(s).map(Option::Some))
            .map_err(|_| Error::BadNames)
    }

    fn init_state(rmlvo: RMLVO) -> Result<KbState, Error> {
        let mut state = KbState::new()?;

        let rules = to_cstring(rmlvo.rules)?;
        let model = to_cstring(rmlvo.model)?;
        let layout = to_cstring(rmlvo.layout)?;
        let variant = to_cstring(rmlvo.variant)?;
        let options = to_cstring(rmlvo.options)?;

        let xkb_names = ffi::xkb_rule_names {
            rules: rules.map_or(ptr::null(), |s| s.as_ptr()),
            model: model.map_or(ptr::null(), |s| s.as_ptr()),
            layout: layout.map_or(ptr::null(), |s| s.as_ptr()),
            variant: variant.map_or(ptr::null(), |s| s.as_ptr()),
            options: options.map_or(ptr::null(), |s| s.as_ptr()),
        };

        unsafe {
            state.init_with_rmlvo(xkb_names)?;
        }

        state.locked = true;
        Ok(state)
    }

    match init_state(rmlvo) {
        Ok(state) => Ok(implement_kbd(keyboard, state, implementation)),
        Err(error) => return Err((error, keyboard)),
    }
}

fn implement_kbd<Impl>(
    kbd: NewProxy<wl_keyboard::WlKeyboard>,
    mut state: KbState,
    mut user_impl: Impl,
) -> Proxy<wl_keyboard::WlKeyboard>
where
    for<'a> Impl: Implementation<Proxy<wl_keyboard::WlKeyboard>, Event<'a>> + Send,
{
    kbd.implement(
        move |event: wl_keyboard::Event, proxy: Proxy<wl_keyboard::WlKeyboard>| {
            match event {
                wl_keyboard::Event::Keymap { format, fd, size } => {
                    if state.locked {
                        // state is locked, ignore keymap updates
                        return;
                    }
                    if state.ready() {
                        // new keymap, we first deinit to free resources
                        unsafe {
                            state.de_init();
                        }
                    }
                    match format {
                        wl_keyboard::KeymapFormat::XkbV1 => unsafe {
                            state.init_with_fd(fd, size as usize);
                        },
                        wl_keyboard::KeymapFormat::NoKeymap => {
                            // TODO: how to handle this (hopefully never occuring) case?
                        }
                    }
                }
                wl_keyboard::Event::Enter {
                    serial,
                    surface,
                    keys,
                } => {
                    let rawkeys: &[u32] = unsafe {
                        ::std::slice::from_raw_parts(keys.as_ptr() as *const u32, keys.len() / 4)
                    };
                    let (keys, modifiers) = {
                        let keys: Vec<u32> =
                            rawkeys.iter().map(|k| state.get_one_sym_raw(*k)).collect();
                        (keys, state.mods_state.clone())
                    };
                    user_impl.receive(
                        Event::Enter {
                            serial,
                            surface,
                            modifiers,
                            rawkeys,
                            keysyms: &keys,
                        },
                        proxy,
                    );
                }
                wl_keyboard::Event::Leave { serial, surface } => {
                    user_impl.receive(Event::Leave { serial, surface }, proxy);
                }
                wl_keyboard::Event::Key {
                    serial,
                    time,
                    key,
                    state: key_state,
                } => {
                    let sym = state.get_one_sym_raw(key);
                    let ignore_text = if key_state == wl_keyboard::KeyState::Pressed {
                        state.compose_feed(sym)
                            != Some(ffi::xkb_compose_feed_result::XKB_COMPOSE_FEED_ACCEPTED)
                    } else {
                        true
                    };
                    let utf8 = if ignore_text {
                        None
                    } else if let Some(status) = state.compose_status() {
                        match status {
                            ffi::xkb_compose_status::XKB_COMPOSE_COMPOSED => {
                                state.compose_get_utf8()
                            }
                            ffi::xkb_compose_status::XKB_COMPOSE_NOTHING => state.get_utf8_raw(key),
                            _ => None,
                        }
                    } else {
                        state.get_utf8_raw(key)
                    };
                    let modifiers = state.mods_state.clone();
                    user_impl.receive(
                        Event::Key {
                            serial,
                            time,
                            modifiers,
                            rawkey: key,
                            keysym: sym,
                            state: key_state,
                            utf8,
                        },
                        proxy,
                    );
                }
                wl_keyboard::Event::Modifiers {
                    mods_depressed,
                    mods_latched,
                    mods_locked,
                    group,
                    ..
                } => state.update_modifiers(mods_depressed, mods_latched, mods_locked, group),
                wl_keyboard::Event::RepeatInfo { rate, delay } => {
                    user_impl.receive(Event::RepeatInfo { rate, delay }, proxy);
                }
            }
        },
    )
}
