use std::os::raw::c_void;

use crate::{game::engine::ENetMode, sdebug, sinfo, tools::hook_globals::{cli_args, globals}, ue::{FName, UFunction, UObject, UStruct}};
use crate::resolvers::unchained_integration::*;

// Desync patch
// FIXME: Add conditionals to CREATE_HOOK?
define_pattern_resolver!(UNetDriver_GetNetMode, [
    "48 83 EC 28 48 8B 01 ?? ?? ?? ?? ?? ?? 84 C0 ?? ?? 33 C0 38 ?? ?? ?? ?? 02 0F 95 C0 FF C0 48 83 C4",
]);
CREATE_PATCH!(UNetDriver_GetNetMode, BYTES, &[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3], IF { cli_args().apply_desync_patch });  // mov eax, 1; ret
// CREATE_HOOK!(UNetDriver_GetNetMode, { || cli_args().apply_desync_patch }, NONE, ENetMode, (this_ptr: *mut c_void), {
//     return ENetMode::DEDICATED_SERVER;
//     let mode = CALL_ORIGINAL!(UNetDriver_GetNetMode(this_ptr));
//     match mode {
//         ENetMode::LISTEN_SERVER => ENetMode::DEDICATED_SERVER,
//         _ => mode
//     }
// });

// TODO: is this needed still?
// Blocks SetCameraMode from being executed
define_pattern_resolver!(SetCameraMode, [
    "48 89 5C 24 08 57 48 83 EC 20 48 8B 81 F0 02 00 00 48 8B DA 48 8B F9 48 85 C0 ?? ?? 48 89 90 68 02 00 00",
]);
// void __thiscall APlayerController::SetCameraMode(APlayerController *this,FName param_1)
CREATE_HOOK!(SetCameraMode, { || cli_args().apply_desync_patch }, NONE, (), (this_ptr: *mut c_void, param_1: FName), {
    sinfo!(f; "SetCameraMode: {param_1}");
    return;
});

// APlayerController::DedicatedServerInit(undefined8 param_1)
define_pattern_resolver!(DedicatedServerInit, [
   "41 56 48 83 EC 30 48 89 6C 24 48 48 89 74 24 50 48 89 7C 24 28 4C 89 7C 24 20 4C 8B F9 ?? ?? ?? ?? ?? 48 8B C8 4C 8B F0"
]);
CREATE_HOOK!(DedicatedServerInit, ACTIVE, NONE, (), (this_ptr: *mut c_void), {});

// Block the ClientSetCameraMode event spam (deadlock)
//
// Under --desync-patch, ClientSetCameraMode feeds an unbounded event loop: loading a Team
// Objective map ends in EXCEPTION_STACK_OVERFLOW a few seconds later. The hook is therefore
// installed whenever --desync-patch is given.
//
// The same hook carries a per-thread depth counter far above legitimate Blueprint nesting, so a
// further event loop of this kind becomes a dropped event plus an error log naming it instead of
// a stack overflow.
thread_local! {
    static PROCESS_EVENT_DEPTH: std::cell::Cell<u32> = std::cell::Cell::new(0);
    // FName comparison index -> "is ClientSetCameraMode". Bounded by the number of distinct UFunctions.
    static CAMERA_MODE_INDEX_CACHE: std::cell::RefCell<std::collections::HashMap<u32, bool>> = std::cell::RefCell::new(std::collections::HashMap::new());
}
// UE's own script recursion limit is 120; native+script nesting in normal play stays well below
// this. A runaway loop reaches it within milliseconds.
const PROCESS_EVENT_MAX_DEPTH: u32 = 300;

fn process_event_names(object: *mut UObject, function: *mut UFunction) -> (String, String) {
    let func_name = unsafe { (*function).ustruct.ufield.uobject.uobject_base_utility.uobject_base.name_private.to_string() };
    let mut class_name = String::new();
    if !object.is_null() {
        let current_class = unsafe { (*object).uobject_base_utility.uobject_base.class_private as *const UStruct };
        if !current_class.is_null() {
            class_name = unsafe { (*current_class).ufield.uobject.uobject_base_utility.uobject_base.name_private.to_string() };
        }
    }
    (class_name, func_name)
}

define_pattern_resolver!(ProcessEvent,["40 55 56 57 41 54 41 55 41 56 41 57 48 81 EC F0 00 00 00 48 8D 6C 24 30 48 89 9D 18 01"]);
CREATE_HOOK!(ProcessEvent, { || cli_args().apply_desync_patch }, NONE, (), (
    object: *mut UObject,
    function: *mut UFunction,
    params: *mut c_void
),{
    if function.is_null() {
        return CALL_ORIGINAL!(ProcessEvent(object, function, params));
    }
    let depth = PROCESS_EVENT_DEPTH.with(|d| d.get());
    if depth >= PROCESS_EVENT_MAX_DEPTH {
        let (class_name, func_name) = process_event_names(object, function);
        log::error!(target: "ProcessEvent", "recursion depth {depth} reached; dropping {class_name}::{func_name} to break the loop");
        return;
    }
    // Every event passes here, so decide "is this ClientSetCameraMode?" by FName index, with the
    // string conversion done once per distinct function and remembered.
    let index = unsafe { (*function).ustruct.ufield.uobject.uobject_base_utility.uobject_base.name_private.comparison_index.value };
    let blocked = CAMERA_MODE_INDEX_CACHE.with(|cache| {
        *cache.borrow_mut().entry(index).or_insert_with(|| {
            let (_, func_name) = process_event_names(std::ptr::null_mut(), function);
            func_name == "ClientSetCameraMode"
        })
    });
    if blocked {
        sdebug!(f; "ClientSetCameraMode blocked");
        return;
    }
    PROCESS_EVENT_DEPTH.with(|d| d.set(depth + 1));
    struct DepthGuard;
    impl Drop for DepthGuard { fn drop(&mut self) { PROCESS_EVENT_DEPTH.with(|d| d.set(d.get().saturating_sub(1))); } }
    let _guard = DepthGuard;
    CALL_ORIGINAL!(ProcessEvent(object, function, params))
});

// Desync for listen server
// FIXME: This may break map objectives, but fixes(?) desync
define_pattern_resolver!(UGameplay_IsDedicatedServer, [
    "48 83 EC 28 48 85 C9 ?? ?? BA 01 00 00 00 ?? ?? ?? ?? ?? 48 85 C0 ?? ?? 48 8B C8 ?? ?? ?? ?? ?? 83 F8 01 0F 94 C0 48",
]);
CREATE_HOOK!(UGameplay_IsDedicatedServer, { || cli_args().playable_listen }, NONE, bool, (param_1: u64),{
    if let Some(world) = globals().world() {
        let mode = unsafe { o_InternalGetNetMode.call(world) };
        if matches!(mode, ENetMode::DEDICATED_SERVER | ENetMode::LISTEN_SERVER) {
            #[cfg(feature="verbose_hooks")]
            crate::sinfo!(f; "Overriding IsDedicatedServer");
            return true;
        }
    }

    CALL_ORIGINAL!(UGameplay_IsDedicatedServer(param_1))
});
