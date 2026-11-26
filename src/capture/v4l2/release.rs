//! Libération des ressources `v4l` sans propager leurs paniques.
//!
//! Les `Drop` de `v4l` 0.14 paniquent sur certaines erreurs (`STREAMOFF` ou `munmap`
//! autres que `ENODEV`, `close` en échec). Une telle panique dans le thread de
//! capture arrêterait l'application ; on l'intercepte et on la journalise, car la
//! ressource est de toute façon perdue et la source sera recréée.

use std::panic::{self, AssertUnwindSafe};

/// Ressource `v4l` détruite sous `catch_unwind` (**RAII**).
///
/// Limite : si la destruction panique **pendant** le déroulement d'une première
/// panique (ex. `Drop` du flux puis de l'`Arena`), Rust interrompt le processus
/// (`abort`) ; aucun code ne peut l'empêcher. Nécessite `panic = "unwind"` (défaut).
pub(crate) struct Released<T> {
    value: Option<T>,
    what: &'static str,
}

impl<T> Released<T> {
    /// Enveloppe `value` ; `what` nomme la ressource dans les logs.
    pub(crate) fn new(value: T, what: &'static str) -> Self {
        Self {
            value: Some(value),
            what,
        }
    }

    /// Accès partagé ; `None` uniquement pendant la destruction.
    pub(crate) fn get(&self) -> Option<&T> {
        self.value.as_ref()
    }

    /// Accès exclusif ; `None` uniquement pendant la destruction.
    pub(crate) fn get_mut(&mut self) -> Option<&mut T> {
        self.value.as_mut()
    }
}

impl<T> Drop for Released<T> {
    fn drop(&mut self) {
        let value = self.value.take();
        if panic::catch_unwind(AssertUnwindSafe(move || drop(value))).is_err() {
            // Le message de la panique a déjà été écrit par le gestionnaire par défaut.
            tracing::warn!(
                resource = self.what,
                "releasing the video device panicked inside v4l; resource dropped anyway"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use super::*;

    struct PanicsOnDrop(Arc<AtomicBool>);

    impl Drop for PanicsOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
            panic!("simulated STREAMOFF failure");
        }
    }

    #[test]
    fn a_panicking_drop_is_contained() {
        let dropped = Arc::new(AtomicBool::new(false));
        let released = Released::new(PanicsOnDrop(Arc::clone(&dropped)), "test resource");
        assert!(released.get().is_some());
        drop(released);
        assert!(dropped.load(Ordering::SeqCst));
    }
}
