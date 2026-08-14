//! Reads (and watches) the desktop's light/dark preference through the XDG
//! Desktop Portal, for the `mono-auto` icon style.
//!
//! The value lives at `org.freedesktop.portal.Settings` on
//! `org.freedesktop.portal.Desktop`, namespace `org.freedesktop.appearance`,
//! key `color-scheme`: `1` = "prefer dark", `2` = "prefer light", `0` = "no
//! preference". `ReadOne` returns it as a single variant; the older `Read`
//! double-wraps it (`v` containing `v` containing `u`), so
//! [`scheme_from_value`] peels however many layers it finds.
//!
//! Everything here degrades to "assume a dark UI" — no portal, no session bus,
//! a D-Bus error, an unreadable value: the icon then looks the way it would
//! under `mono-dark`, and a user on a light desktop can pin `mono-light`. The
//! watcher runs on its own thread and never panics; the worst case is that it
//! returns early and the appearance simply stops following the desktop.
//!
//! Only [`scheme_from_value`] and [`dark_ui_from_scheme`] are unit-tested: they
//! carry all the decision-making, and they need neither a bus nor a portal. The
//! D-Bus plumbing around them is verified by running the tray on a real
//! desktop.

use zbus::zvariant::Value;

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.portal.Settings";
const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "color-scheme";

/// Turns a `color-scheme` value into "is the user's UI dark?".
///
/// `1` means the user prefers dark. `0` (no preference) and `2` (prefer light)
/// both mean light. `None` — no portal, an error, or a value we could not read
/// — assumes dark, which is the more common panel background and the documented
/// fallback. An unknown future value is treated as light rather than guessed
/// at, since only "prefer dark" has ever been `1`.
pub fn dark_ui_from_scheme(scheme: Option<u32>) -> bool {
    match scheme {
        Some(1) => true,
        Some(_) => false,
        None => true,
    }
}

/// Extracts the `u` out of a portal reply, unwrapping however many nested
/// variants it arrived in (`ReadOne` sends one, `Read` sends two).
///
/// Integer types other than `u32` are accepted too: the spec says `u`, but a
/// portal implementation that answers with a different width is still telling
/// us something we can use, and refusing it would silently fall back to dark.
pub fn scheme_from_value(value: &Value<'_>) -> Option<u32> {
    let mut current = value;
    // Bounded rather than unbounded recursion: a pathological reply nesting
    // variants forever must not hang the listener thread.
    for _ in 0..8 {
        match current {
            Value::Value(inner) => current = inner,
            Value::U8(n) => return Some(u32::from(*n)),
            Value::U16(n) => return Some(u32::from(*n)),
            Value::U32(n) => return Some(*n),
            Value::U64(n) => return u32::try_from(*n).ok(),
            Value::I16(n) => return u32::try_from(*n).ok(),
            Value::I32(n) => return u32::try_from(*n).ok(),
            Value::I64(n) => return u32::try_from(*n).ok(),
            _ => return None,
        }
    }
    None
}

/// Starts a thread that reports the desktop's dark/light preference: once at
/// startup, then again on every `SettingChanged` for the appearance key.
///
/// `on_change` is called from that thread, so it must be cheap and must not
/// panic. A failure anywhere in the D-Bus path ends the thread quietly after
/// reporting the dark fallback — the tray keeps running, `mono-auto` just stops
/// tracking the desktop (the pinned styles are unaffected).
pub fn spawn_watcher<F>(on_change: F)
where
    F: Fn(bool) + Send + 'static,
{
    std::thread::Builder::new()
        .name("portal-appearance".into())
        .spawn(move || watch(on_change))
        // A thread that cannot be spawned is exactly as bad as a missing
        // portal, and is handled the same way: nothing happens, the default
        // (dark) stands.
        .map(|_handle| ())
        .unwrap_or(())
}

fn watch<F>(on_change: F)
where
    F: Fn(bool) + Send + 'static,
{
    let Ok(connection) = zbus::blocking::Connection::session() else {
        on_change(dark_ui_from_scheme(None));
        return;
    };
    let Ok(proxy) = zbus::blocking::Proxy::new(&connection, DESTINATION, PATH, INTERFACE) else {
        on_change(dark_ui_from_scheme(None));
        return;
    };

    on_change(dark_ui_from_scheme(read_scheme(&proxy)));

    // Subscribing before/after the initial read makes no practical difference:
    // a change racing the read only means one redundant re-render.
    let Ok(signals) = proxy.receive_signal("SettingChanged") else {
        return;
    };
    for message in signals {
        let body = message.body();
        let Ok((namespace, key, value)) = body.deserialize::<(String, String, Value<'_>)>() else {
            continue;
        };
        if namespace != NAMESPACE || key != KEY {
            continue;
        }
        on_change(dark_ui_from_scheme(scheme_from_value(&value)));
    }
}

/// One `ReadOne` call, falling back to `Read` for portals too old to have it.
fn read_scheme(proxy: &zbus::blocking::Proxy<'_>) -> Option<u32> {
    let args = (NAMESPACE, KEY);
    if let Ok(value) = proxy.call::<_, _, zbus::zvariant::OwnedValue>("ReadOne", &args)
        && let Some(scheme) = scheme_from_value(&value)
    {
        return Some(scheme);
    }
    let value = proxy
        .call::<_, _, zbus::zvariant::OwnedValue>("Read", &args)
        .ok()?;
    scheme_from_value(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_is_dark_zero_and_two_are_light() {
        assert!(dark_ui_from_scheme(Some(1)));
        assert!(!dark_ui_from_scheme(Some(0)));
        assert!(!dark_ui_from_scheme(Some(2)));
    }

    #[test]
    fn an_absent_or_failed_read_assumes_a_dark_ui() {
        assert!(dark_ui_from_scheme(None));
    }

    #[test]
    fn an_unknown_scheme_value_is_treated_as_light() {
        assert!(!dark_ui_from_scheme(Some(3)));
        assert!(!dark_ui_from_scheme(Some(u32::MAX)));
    }

    /// Wraps a value in one more variant layer. Written out rather than via
    /// `Value::from`, which collapses `Value` into itself instead of nesting.
    fn variant(inner: Value<'static>) -> Value<'static> {
        Value::Value(Box::new(inner))
    }

    #[test]
    fn reads_the_single_variant_readone_returns() {
        assert_eq!(scheme_from_value(&variant(Value::U32(1))), Some(1));
    }

    #[test]
    fn reads_the_double_wrapped_variant_read_returns() {
        assert_eq!(
            scheme_from_value(&variant(variant(Value::U32(2)))),
            Some(2)
        );
    }

    #[test]
    fn reads_a_bare_unwrapped_integer() {
        assert_eq!(scheme_from_value(&Value::U32(0)), Some(0));
    }

    #[test]
    fn accepts_other_integer_widths() {
        assert_eq!(scheme_from_value(&Value::U8(1)), Some(1));
        assert_eq!(scheme_from_value(&Value::I32(2)), Some(2));
        assert_eq!(scheme_from_value(&Value::U64(1)), Some(1));
    }

    #[test]
    fn rejects_values_that_are_not_numbers_and_negative_ones() {
        assert_eq!(scheme_from_value(&Value::from("dark")), None);
        assert_eq!(scheme_from_value(&Value::Bool(true)), None);
        assert_eq!(scheme_from_value(&Value::I32(-1)), None);
        // ...and a rejected read still lands on the dark fallback.
        assert!(dark_ui_from_scheme(scheme_from_value(&Value::Bool(true))));
    }
}
