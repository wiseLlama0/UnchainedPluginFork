use std::os::raw::c_void;

use crate::events::models::{CombatActor, Damage, DamageSource, GameEvent, Kill};
use crate::features::events::EVENT_SYSTEM;
use crate::game::chivalry2::{AController, APlayerState, ATBLCharacter, FDeathDamageTakenEvent, FDamageTakenEvent, PlayerFlags};
use crate::tools::hook_globals::globals;
use crate::ue::{UObject, UStruct};

fn object_class_name(ptr: *mut c_void) -> Option<String> {
    if !is_live_object(ptr) { return None; }
    let obj = unsafe { (ptr as *const UObject).as_ref() }?;
    let class = unsafe { obj.uobject_base_utility.uobject_base.class_private.as_ref() }?;
    Some(class.ustruct.ufield.uobject.uobject_base_utility.uobject_base.name_private.to_string())
}

fn object_name(ptr: *mut c_void) -> Option<String> {
    if !is_live_object(ptr) { return None; }
    let obj = unsafe { (ptr as *const UObject).as_ref() }?;
    Some(obj.uobject_base_utility.uobject_base.name_private.to_string())
}

fn object_display_name(ptr: *mut c_void) -> Option<String> {
    object_name(ptr)
        .or_else(|| object_class_name(ptr))
        .map(|name| normalize_display_name(&name))
        .filter(|name| !name.trim().is_empty())
}

/// Cheap pre-filter: non-null, 8-byte aligned and inside the canonical user-mode range. Not proof
/// that an object lives there (0x100000000 passes); use is_live_object before reading through a
/// pointer.
fn looks_like_pointer(p: *const c_void) -> bool {
    let v = p as usize;
    v >= 0x10000 && v < 0x0000_7FFF_FFFF_0000 && v % 8 == 0
}

/// True when `p` is a live UObject according to GUObjectArray (FUObjectArray::is_live): catches
/// unmapped garbage and dangling pointers to destroyed objects alike.
fn is_live_object(p: *const c_void) -> bool {
    looks_like_pointer(p) && unsafe { globals().guobject_array_unchecked() }.is_live(p.cast())
}

fn object_inherits_from(ptr: *mut c_void, needle: &str) -> bool {
    if !is_live_object(ptr) { return false; }
    let mut curr = unsafe { (ptr as *const UObject).as_ref() }
        .and_then(|o| unsafe { o.uobject_base_utility.uobject_base.class_private.as_ref() })
        .map(|c| &c.ustruct as *const UStruct);

    let mut depth = 0;
    while let Some(s) = unsafe { curr.and_then(|p| p.as_ref()) } {
        // Exact class-name match: a substring match would let "Character" accept classes such as
        // CharacterDeathcam_C, which do not have the ATBLCharacter layout.
        if s.ufield.uobject.uobject_base_utility.uobject_base.name_private.to_string() == needle {
            return true;
        }
        depth += 1;
        if depth > 32 { return false; }
        curr = looks_like_pointer(s.super_struct.cast()).then_some(s.super_struct);
    }
    false
}

fn trim_instance_suffix(name: &str) -> String {
    match name.rsplit_once('_') {
        Some((base, suffix)) if suffix.chars().all(|c| c.is_ascii_digit()) => base.to_string(),
        _ => name.to_string(),
    }
}

fn normalize_display_name(name: &str) -> String {
    let without_instance = trim_instance_suffix(name);
    without_instance.trim_end_matches("_C").to_string()
}

fn fallback_combat_actor(ptr: *mut c_void) -> Option<CombatActor> {
    let cls = object_class_name(ptr)?;
    let obj = object_name(ptr).unwrap_or_else(|| cls.clone());
    let is_bot = cls.to_lowercase().contains("bot") || obj.to_lowercase().contains("bot");

    Some(CombatActor::new(normalize_display_name(&obj), is_bot))
}

/// `via` names the field the pointer came from, for the warning when it is not a live object.
fn combat_actor_from_player_state(player_state: *mut APlayerState, via: &str) -> Option<CombatActor> {
    if player_state.is_null() { return None; }
    if !is_live_object(player_state.cast()) {
        crate::swarn!(f; "ignoring PlayerState {:p} from {}: not a live object", player_state, via);
        return None;
    }
    let ps = unsafe { player_state.as_ref() }?;
    let name = ps.player_name_private.copy_to_string().ok()?;
    (!name.trim().is_empty()).then(|| {
        CombatActor::new(name.trim().to_string(), ps.player_flags.contains(PlayerFlags::IS_A_BOT))
    })
}

fn combat_actor_from_actor(ptr: *mut c_void) -> Option<CombatActor> {
    if ptr.is_null() { return None; }
    if !is_live_object(ptr) {
        crate::swarn!(f; "ignoring combat actor {:p}: not a live object", ptr);
        return None;
    }

    let is_tbl_character = object_inherits_from(ptr, "TBLCharacter");
    if is_tbl_character || object_inherits_from(ptr, "Character") {
        // The APawn fields are valid on any Character; the cast to ATBLCharacter is only read
        // further for LastPlayerState, which exists on ATBLCharacter alone and lies past the end
        // of a plain ACharacter.
        let char = unsafe { (ptr as *mut ATBLCharacter).as_ref() }?;
        let pawn = &char.base_tbl_character_base.base_character.base_pawn;

        return combat_actor_from_player_state(pawn.player_state as *mut APlayerState, "Pawn.PlayerState")
            .or_else(|| {
                if !is_live_object(pawn.controller) { return None; }
                let controller = unsafe { pawn.controller.cast::<AController>().as_ref() }?;
                combat_actor_from_player_state(controller.player_state, "Pawn.Controller.PlayerState")
            })
            .or_else(|| {
                if !is_tbl_character { return None; }
                combat_actor_from_player_state(char.last_player_state as *mut APlayerState, "TBLCharacter.LastPlayerState")
            })
            .or_else(|| fallback_combat_actor(ptr));
    }

    if object_inherits_from(ptr, "Controller") || object_inherits_from(ptr, "PlayerController") || object_inherits_from(ptr, "AIController") || object_inherits_from(ptr, "TBLPlayerController") {
        let controller = unsafe { (ptr as *mut AController).as_ref() }?;
        return combat_actor_from_player_state(controller.player_state, "Controller.PlayerState")
            .or_else(|| fallback_combat_actor(ptr));
    }

    if object_inherits_from(ptr, "PlayerState") {
        return combat_actor_from_player_state(ptr.cast(), "PlayerState")
            .or_else(|| fallback_combat_actor(ptr));
    }

    fallback_combat_actor(ptr)
}

fn resolve_killer_from_damage_event(damage_event: &FDamageTakenEvent) -> Option<CombatActor> {
    combat_actor_from_actor(damage_event.damage_instigator.cast())
        .or_else(|| combat_actor_from_actor(damage_event.damage_causer.cast()))
}

fn resolve_killers_from_death_event(death_event: &FDeathDamageTakenEvent) -> Vec<CombatActor> {
    let mut resolved: Vec<_> = death_event.killers.as_slice().iter()
        .filter_map(|&ptr| combat_actor_from_actor(ptr))
        .collect();

    if resolved.is_empty() {
        if let Some(actor) = resolve_killer_from_damage_event(&death_event.damage_taken) {
            resolved.push(actor);
        }
    }
    resolved
}

fn resolve_victim(damage_event: &FDamageTakenEvent) -> Option<CombatActor> {
    combat_actor_from_actor(damage_event.damage_taker.cast())
}

fn damage_source_name(damage_event: &FDamageTakenEvent) -> String {
    // Prefer concrete weapon/projectile identity over abstract UDamageSource.
    let candidates = [
        ("inventory_item", damage_event.inventory_item),
        ("projectile", damage_event.projectile),
        ("damage_source", damage_event.damage_source.cast::<c_void>()),
    ];

    for (kind, ptr) in candidates {
        if ptr.is_null() {
            continue;
        }
        if let Some(name) = object_display_name(ptr) {
            if kind != "damage_source" {
                crate::strace!(f; "OnKilled source resolved via {}='{}'", kind, name);
            }
            return name;
        }
    }

    "UnknownDamageSource".to_string()
}

fn attack_type_from_damage_event(damage_event: &FDamageTakenEvent) -> String {
    if damage_event.b_suicide { return "Suicide".into(); }
    if damage_event.b_back_stab { return "Backstab".into(); }
    if damage_event.b_entered_kill_volume { return "KillVolume".into(); }
    "Damage".into()
}

define_pattern_resolver!(ATBLCharacter__OnKilled, [
    "4C 8B DC 55 41 55 41 57 49 8D AB ?? ?? ?? ?? 48 81 EC 30 03 00 00"
]);
CREATE_HOOK!(ATBLCharacter__OnKilled, ACTIVE, NONE, (), (
    this_ptr: *mut ATBLCharacter,
    damage_event: *const FDeathDamageTakenEvent
), {
    if this_ptr.is_null() || damage_event.is_null() {
        CALL_ORIGINAL!(ATBLCharacter__OnKilled(this_ptr, damage_event));
        return;
    }
    let death_event = unsafe { &*damage_event };
    let damage_taken = &death_event.damage_taken;

    let victim = resolve_victim(damage_taken)
        .unwrap_or_else(|| CombatActor::new("UnknownVictim".into(), false));

    let killers = resolve_killers_from_death_event(death_event);
    let killer = killers.first().cloned().unwrap_or_else(|| CombatActor::new("UnknownKiller".into(), false));

    let source = damage_source_name(damage_taken);
    let kill_reason = death_event.kill_reason.as_str().to_string();

    crate::sdebug!(f; "Kill: {} -> {} [{}] via {} ({:.2} dmg)", killer.name, victim.name, kill_reason, source, damage_taken.damage);

    EVENT_SYSTEM.game_event_publisher.publish(GameEvent::KillEvent(Kill {
        killer: killer.clone(),
        victim: victim.clone(),
        killers: if killers.is_empty() { vec![killer.clone()] } else { killers },
        kill_reason,
        random_seed: death_event.random_seed,
        dead_character_id: death_event.dead_character_id,
        attach_to_projectile: death_event.b_attach_to_projectile,
        source: Damage {
            attacker: killer.name.clone(),
            victim: victim.name.clone(),
            attacker_actor: killer,
            victim_actor: victim,
            damage: DamageSource {
                amount: damage_taken.damage,
                source,
                attack_type: attack_type_from_damage_event(damage_taken),
            },
        },
    }));

    CALL_ORIGINAL!(ATBLCharacter__OnKilled(this_ptr, damage_event));
});


define_pattern_resolver!(ATBLCharacter__OnDamageTaken, [
    "4C 8B DC 55 57 41 55 41 56 49 8D AB ?? ?? ?? ?? 48 81 EC 58 02 00 00"
]);
CREATE_HOOK!(ATBLCharacter__OnDamageTaken, ACTIVE, NONE, (), (
    this_ptr: *mut ATBLCharacter,
    damage_event: *const FDamageTakenEvent
), {
    if this_ptr.is_null() || damage_event.is_null() {
        CALL_ORIGINAL!(ATBLCharacter__OnDamageTaken(this_ptr, damage_event));
        return;
    }

    let damage_taken = unsafe { &*damage_event };
    let attacker = resolve_killer_from_damage_event(damage_taken)
        .unwrap_or_else(|| CombatActor::new("UnknownAttacker".into(), false));
    let victim = resolve_victim(damage_taken)
        .or_else(|| combat_actor_from_actor(this_ptr.cast()))
        .unwrap_or_else(|| CombatActor::new("UnknownVictim".into(), false));

    let source = damage_source_name(damage_taken);
    let attack_type = attack_type_from_damage_event(damage_taken);

    crate::sdebug!(
        f;
        "Damage: {} -> {} via {} ({:.2} dmg, type={})",
        attacker.name,
        victim.name,
        source,
        damage_taken.damage,
        attack_type
    );

    EVENT_SYSTEM.game_event_publisher.publish(GameEvent::DamageEvent(Damage {
        attacker: attacker.name.clone(),
        victim: victim.name.clone(),
        attacker_actor: attacker,
        victim_actor: victim,
        damage: DamageSource {
            amount: damage_taken.damage,
            source,
            attack_type,
        },
    }));

    CALL_ORIGINAL!(ATBLCharacter__OnDamageTaken(this_ptr, damage_event));
});
