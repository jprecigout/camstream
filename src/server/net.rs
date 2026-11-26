//! Options de socket TCP appliquées à chaque connexion acceptée (ADR 0004, décision 7).
//!
//! Un client disparu sans `RST` (Wi-Fi coupé, téléphone en veille) bloquerait son
//! écriture indéfiniment, garderait sa place et maintiendrait la webcam allumée.
//! `SO_KEEPALIVE` et `TCP_USER_TIMEOUT` laissent le noyau fermer ces connexions ;
//! hyper détruit alors le corps MJPEG (désabonnement, place libérée).

use std::io;
use std::time::Duration;

use socket2::{SockRef, TcpKeepalive};

/// Options appliquées à chaque socket acceptée (via `socket2::SockRef`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpOptions {
    /// Inactivité avant la première sonde keepalive.
    pub keepalive_idle: Duration,
    /// Intervalle entre sondes keepalive.
    pub keepalive_interval: Duration,
    /// Nombre de sondes keepalive sans réponse avant fermeture (`TCP_KEEPCNT`).
    pub keepalive_retries: u32,
    /// Durée maximale de données non acquittées avant fermeture par le noyau
    /// (`TCP_USER_TIMEOUT`, Linux uniquement).
    pub user_timeout: Duration,
    /// Désactive l'algorithme de Nagle (`TCP_NODELAY`) pour envoyer chaque partie
    /// MJPEG sans attendre.
    pub nodelay: bool,
}

impl Default for TcpOptions {
    /// Keepalive 30 s / 10 s / 3 sondes, `TCP_USER_TIMEOUT` 30 s, `TCP_NODELAY`.
    fn default() -> Self {
        Self {
            keepalive_idle: Duration::from_secs(30),
            keepalive_interval: Duration::from_secs(10),
            keepalive_retries: 3,
            user_timeout: Duration::from_secs(30),
            nodelay: true,
        }
    }
}

/// Applique les options à une socket TCP connectée.
///
/// Une erreur n'est pas fatale pour l'appelant : la connexion reste servie sans
/// les options (le serveur la journalise en `debug`).
pub fn apply_tcp_options(socket: SockRef<'_>, options: &TcpOptions) -> io::Result<()> {
    let keepalive = TcpKeepalive::new()
        .with_time(options.keepalive_idle)
        .with_interval(options.keepalive_interval);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let keepalive = keepalive.with_retries(options.keepalive_retries);
    socket.set_tcp_keepalive(&keepalive)?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    socket.set_tcp_user_timeout(Some(options.user_timeout))?;
    socket.set_tcp_nodelay(options.nodelay)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};

    use super::*;

    #[test]
    fn options_are_applied_to_a_connected_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let options = TcpOptions::default();
        apply_tcp_options(SockRef::from(&server), &options).unwrap();

        let socket = SockRef::from(&server);
        assert!(socket.keepalive().unwrap());
        assert!(socket.tcp_nodelay().unwrap());
        #[cfg(target_os = "linux")]
        {
            assert_eq!(socket.tcp_keepalive_time().unwrap(), options.keepalive_idle);
            assert_eq!(
                socket.tcp_keepalive_interval().unwrap(),
                options.keepalive_interval
            );
            assert_eq!(
                socket.tcp_keepalive_retries().unwrap(),
                options.keepalive_retries
            );
            assert_eq!(
                socket.tcp_user_timeout().unwrap(),
                Some(options.user_timeout)
            );
        }
        drop(client);
    }
}
