//! One preference domain for packaged apps and standalone development binaries.

use objc2::msg_send_id;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::NSString;

pub(crate) const DOMAIN: &str = "com.jxz.cocoa-way";

pub(crate) fn app_defaults() -> Option<Retained<AnyObject>> {
    let bundle_class = AnyClass::get("NSBundle")?;
    let identifier: Option<Retained<NSString>> = unsafe {
        let bundle: Retained<AnyObject> = msg_send_id![bundle_class, mainBundle];
        msg_send_id![&*bundle, bundleIdentifier]
    };
    defaults_for_suite(
        DOMAIN,
        identifier
            .as_ref()
            .map(|value| value.to_string())
            .as_deref(),
    )
}

fn defaults_for_suite(
    suite: &str,
    application_identifier: Option<&str>,
) -> Option<Retained<AnyObject>> {
    let class = AnyClass::get("NSUserDefaults")?;
    unsafe {
        // An application's own bundle identifier is not a valid extra suite.
        if application_identifier == Some(suite) {
            return msg_send_id![class, standardUserDefaults];
        }
        let allocated = msg_send_id![class, alloc];
        msg_send_id![allocated, initWithSuiteName: &*NSString::from_str(suite)]
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use objc2::msg_send;

    pub(crate) struct OwnedSuite(pub(crate) String);

    impl OwnedSuite {
        pub(crate) fn new() -> Self {
            Self(format!(
                "com.jxz.cocoa-way.preferences-test.{}.{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
        }

        pub(crate) fn defaults(&self) -> Retained<AnyObject> {
            defaults_for_suite(&self.0, None).unwrap()
        }
    }

    impl Drop for OwnedSuite {
        fn drop(&mut self) {
            let defaults = self.defaults();
            unsafe {
                let _: () = msg_send![&*defaults, removePersistentDomainForName: &*NSString::from_str(&self.0)];
                let _: bool = msg_send![&*defaults, synchronize];
            }
        }
    }

    #[test]
    fn current_app_identifier_uses_the_standard_defaults_instance() {
        let selected = defaults_for_suite(DOMAIN, Some(DOMAIN)).unwrap();
        let class = AnyClass::get("NSUserDefaults").unwrap();
        let standard: Retained<AnyObject> = unsafe { msg_send_id![class, standardUserDefaults] };
        assert_eq!(Retained::as_ptr(&selected), Retained::as_ptr(&standard));
        assert!(app_defaults().is_some());
    }

    #[test]
    fn independent_suite_persists_without_modifying_the_user_language() {
        let suite = OwnedSuite::new();
        let first = defaults_for_suite(&suite.0, Some("different.application")).unwrap();
        unsafe {
            let _: () = msg_send![&*first, setObject: &*NSString::from_str("en") forKey: &*NSString::from_str("language")];
            let saved: bool = msg_send![&*first, synchronize];
            assert!(saved);
            let next = suite.defaults();
            let value: Retained<NSString> =
                msg_send_id![&*next, stringForKey: &*NSString::from_str("language")];
            assert_eq!(value.to_string(), "en");
        }
    }
}
