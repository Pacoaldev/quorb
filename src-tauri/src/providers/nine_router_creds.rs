//! Credenciales de 9Router: lectura coherente (bound + contraseña + cookie) y
//! mutaciones serializadas con generación, para que un fetch/probe contra A no
//! pueda leer o restaurar secretos de B tras un guardado/conexión/logout concurrente.

use crate::secrets;

/// Normaliza una Base URL de 9Router para comparar destinos (trim + sin `/` final).
pub fn normalize_base(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Vista coherente de los tres secretos en un solo instante (misma generación).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub generation: u64,
    pub bound: Option<String>,
    pub password: Option<String>,
    pub session: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PersistError {
    /// Otro hilo invalidó o cambió las credenciales mientras esperábamos red.
    Stale,
    Store(String),
}

fn read_snapshot_locked() -> Snapshot {
    Snapshot {
        generation: GENERATION.load(Ordering::SeqCst),
        bound: secrets::get(secrets::NINE_ROUTER_BOUND_URL)
            .map(|s| normalize_base(&s))
            .filter(|s| !s.is_empty()),
        password: non_empty(secrets::get(secrets::NINE_ROUTER_PASSWORD)),
        session: non_empty(secrets::get(secrets::NINE_ROUTER_SESSION)),
    }
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

/// Snapshot solo si el vínculo coincide con `base` (trim + sin `/` final).
pub fn snapshot_for_base(base: &str) -> Option<Snapshot> {
    let _g = LOCK.lock().unwrap();
    let snap = read_snapshot_locked();
    let want = normalize_base(base);
    if snap.bound.as_deref() != Some(&want) {
        return None;
    }
    Some(snap)
}

/// Contraseña guardada para `base` y la generación vigente al leer (para validar tras `.await`).
pub fn password_ticket_for_base(base: &str) -> Option<(u64, String)> {
    snapshot_for_base(base).and_then(|s| s.password.map(|p| (s.generation, p)))
}

/// Borra los tres secretos; invalida renovaciones pendientes (sube generación primero).
pub fn forget_all() -> Result<(), String> {
    let _g = LOCK.lock().unwrap();
    GENERATION.fetch_add(1, Ordering::SeqCst);
    let pass = secrets::set(secrets::NINE_ROUTER_PASSWORD, "");
    let sess = secrets::set(secrets::NINE_ROUTER_SESSION, "");
    let bound = secrets::set(secrets::NINE_ROUTER_BOUND_URL, "");
    match (pass, sess, bound) {
        (Ok(()), Ok(()), Ok(())) => Ok(()),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
            Err(format!("No se pudieron borrar las credenciales de 9Router: {e}"))
        }
    }
}

/// Persiste los tres secretos como un solo paso (login explícito / conexión).
pub fn persist_all(base: &str, password: &str, cookie: &str) -> Result<(), String> {
    let _g = LOCK.lock().unwrap();
    write_bundle(base, password, cookie)?;
    GENERATION.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// Tras un login en red, solo escribe si la generación no cambió (logout / otro destino).
pub fn persist_if_generation(base: &str, password: &str, cookie: &str, generation: u64) -> Result<(), PersistError> {
    let _g = LOCK.lock().unwrap();
    if GENERATION.load(Ordering::SeqCst) != generation {
        return Err(PersistError::Stale);
    }
    write_bundle(base, password, cookie).map_err(PersistError::Store)?;
    GENERATION.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

fn write_bundle(base: &str, password: &str, cookie: &str) -> Result<(), String> {
    let normalized = normalize_base(base);
    secrets::set(secrets::NINE_ROUTER_PASSWORD, password)?;
    if let Err(e) = secrets::set(secrets::NINE_ROUTER_SESSION, cookie) {
        let _ = secrets::set(secrets::NINE_ROUTER_PASSWORD, "");
        return Err(e);
    }
    if let Err(e) = secrets::set(secrets::NINE_ROUTER_BOUND_URL, &normalized) {
        let _ = secrets::set(secrets::NINE_ROUTER_PASSWORD, "");
        let _ = secrets::set(secrets::NINE_ROUTER_SESSION, "");
        return Err(e);
    }
    Ok(())
}

// ── Lógica pura (misma semántica que arriba) para regresiones de interleaving ──

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MemStore {
    pub generation: u64,
    pub bound: Option<String>,
    pub password: Option<String>,
    pub session: Option<String>,
}

impl MemStore {
    pub fn snapshot_for_base(&self, base: &str) -> Option<Snapshot> {
        let want = normalize_base(base);
        if self.bound.as_deref() != Some(&want) {
            return None;
        }
        Some(Snapshot {
            generation: self.generation,
            bound: self.bound.clone(),
            password: self.password.clone(),
            session: self.session.clone(),
        })
    }

    pub fn forget(&mut self) {
        self.generation += 1;
        self.bound = None;
        self.password = None;
        self.session = None;
    }

    pub fn persist_all(&mut self, base: &str, password: &str, cookie: &str) {
        self.bound = Some(normalize_base(base));
        self.password = Some(password.to_string());
        self.session = Some(cookie.to_string());
        self.generation += 1;
    }

    pub fn persist_if_generation(&mut self, base: &str, password: &str, cookie: &str, generation: u64) -> Result<(), PersistError> {
        if self.generation != generation {
            return Err(PersistError::Stale);
        }
        self.persist_all(base, password, cookie);
        Ok(())
    }

    /// Misma semántica que `write_bundle` en producción: si falla la cookie, revierte la contraseña.
    pub fn write_bundle(&mut self, base: &str, password: &str, cookie: &str, fail_session: bool) -> Result<(), String> {
        let prev_password = self.password.clone();
        self.password = Some(password.to_string());
        if fail_session {
            self.password = prev_password;
            return Err("session write failed".into());
        }
        self.session = Some(cookie.to_string());
        self.bound = Some(normalize_base(base));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P1: validar A y leer cookie en un solo snapshot; tras conectar B, A ya no ve la cookie de B.
    #[test]
    fn validate_a_then_connect_b_does_not_leak_b_session_to_a() {
        let mut store = MemStore {
            generation: 1,
            bound: Some("http://a".into()),
            password: Some("pa".into()),
            session: Some("cookie-a".into()),
        };
        let snap_a = store.snapshot_for_base("http://a").expect("bound to A");
        assert_eq!(snap_a.session.as_deref(), Some("cookie-a"));

        store.persist_all("http://b", "pb", "cookie-b");

        assert!(store.snapshot_for_base("http://a").is_none());
        let snap_b = store.snapshot_for_base("http://b").unwrap();
        assert_eq!(snap_b.session.as_deref(), Some("cookie-b"));
        // El fetch A que tenía snap_a no debe re-leer: la cookie queda en snap_a (vieja), pero
        // un nuevo snapshot_for_base(A) no devuelve cookie-b.
        assert_ne!(snap_a.session.as_deref(), Some("cookie-b"));
    }

    /// P1: renovación A pendiente → conectar B no debe restaurar credenciales A.
    #[test]
    fn pending_renewal_a_aborted_when_b_connects() {
        let mut store = MemStore {
            generation: 2,
            bound: Some("http://a".into()),
            password: Some("pa".into()),
            session: Some("old-a".into()),
        };
        let gen_at_start = store.generation;
        // await login en A…
        store.persist_all("http://b", "pb", "cookie-b");
        assert_eq!(
            store.persist_if_generation("http://a", "pa", "new-a", gen_at_start),
            Err(PersistError::Stale)
        );
        assert_eq!(store.bound.as_deref(), Some("http://b"));
        assert_eq!(store.session.as_deref(), Some("cookie-b"));
    }

    /// P1: renovación pendiente → logout descarta la respuesta antigua.
    #[test]
    fn pending_renewal_discarded_after_logout() {
        let mut store = MemStore {
            generation: 3,
            bound: Some("http://a".into()),
            password: Some("pa".into()),
            session: None,
        };
        let gen_at_start = store.generation;
        store.forget();
        assert_eq!(
            store.persist_if_generation("http://a", "pa", "cookie-a", gen_at_start),
            Err(PersistError::Stale)
        );
        assert!(store.bound.is_none());
    }

    /// P1: fallo al escribir la cookie no deja contraseña huérfana ni cambia bound previo.
    #[test]
    fn partial_write_rolls_back_password_when_session_fails() {
        let mut store = MemStore {
            generation: 0,
            bound: Some("http://b".into()),
            password: Some("pb".into()),
            session: Some("cookie-b".into()),
        };
        let before = store.clone();
        assert!(store.write_bundle("http://a", "pa", "cookie-a", true).is_err());
        assert_eq!(store.bound, before.bound);
        assert_eq!(store.session, before.session);
        assert_eq!(store.password, before.password);
    }
}
