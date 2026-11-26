//! Agrégation des mesures de capture (cadence, tailles, trames invalides).
//!
//! Indépendant de V4L2 : les instants sont fournis par l'appelant, ce qui rend le
//! calcul testable avec des horloges synthétiques.

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::jpeg::JpegInfo;

/// Accumulateur de statistiques sur une série de trames.
#[derive(Debug, Default)]
pub struct CaptureStats {
    frames: u64,
    total_bytes: u64,
    min_bytes: Option<usize>,
    max_bytes: usize,
    incomplete_jpeg: u64,
    without_dht: u64,
    padded: u64,
    sequence_gaps: u64,
    buffer_len: usize,
    first_at: Option<Instant>,
    last_at: Option<Instant>,
    last_sequence: Option<u32>,
}

impl CaptureStats {
    /// Enregistre une trame.
    ///
    /// `buffer_len` est la taille du tampon mmap renvoyé par `v4l` ; `bytes_used` la
    /// taille utile. `sequence` est le numéro de séquence du pilote (détection des
    /// trames perdues par le pilote lui-même).
    pub fn record(
        &mut self,
        at: Instant,
        buffer_len: usize,
        bytes_used: usize,
        sequence: u32,
        jpeg: JpegInfo,
    ) {
        self.frames += 1;
        self.total_bytes += bytes_used as u64;
        self.min_bytes = Some(self.min_bytes.map_or(bytes_used, |m| m.min(bytes_used)));
        self.max_bytes = self.max_bytes.max(bytes_used);
        self.buffer_len = self.buffer_len.max(buffer_len);
        if !jpeg.is_complete() {
            self.incomplete_jpeg += 1;
        }
        if !jpeg.has_dht {
            self.without_dht += 1;
        }
        if jpeg.trailing_padding > 0 {
            self.padded += 1;
        }
        if let Some(previous) = self.last_sequence {
            // Saut de plus d'une unité : le pilote a perdu des trames entre deux lectures.
            let gap = sequence.wrapping_sub(previous).saturating_sub(1);
            self.sequence_gaps += u64::from(gap);
        }
        self.last_sequence = Some(sequence);
        self.first_at.get_or_insert(at);
        self.last_at = Some(at);
    }

    /// Produit le résumé sérialisable.
    pub fn summary(&self) -> CaptureSummary {
        let span = match (self.first_at, self.last_at) {
            (Some(first), Some(last)) => last.saturating_duration_since(first),
            _ => Duration::ZERO,
        };
        // N trames délimitent N - 1 intervalles.
        let fps = if self.frames >= 2 && !span.is_zero() {
            Some((self.frames - 1) as f64 / span.as_secs_f64())
        } else {
            None
        };
        let mean_bytes = if self.frames > 0 {
            Some(self.total_bytes as f64 / self.frames as f64)
        } else {
            None
        };
        CaptureSummary {
            frames: self.frames,
            span_ms: span.as_secs_f64() * 1000.0,
            fps,
            mean_bytes,
            min_bytes: self.min_bytes,
            max_bytes: self.max_bytes,
            buffer_len: self.buffer_len,
            incomplete_jpeg: self.incomplete_jpeg,
            without_dht: self.without_dht,
            padded: self.padded,
            sequence_gaps: self.sequence_gaps,
        }
    }
}

/// Résumé d'une série de trames.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CaptureSummary {
    /// Nombre de trames reçues.
    pub frames: u64,
    /// Durée entre la première et la dernière trame, en millisecondes.
    pub span_ms: f64,
    /// Cadence mesurée (absente avec moins de deux trames).
    pub fps: Option<f64>,
    /// Taille utile moyenne, en octets.
    pub mean_bytes: Option<f64>,
    /// Taille utile minimale.
    pub min_bytes: Option<usize>,
    /// Taille utile maximale.
    pub max_bytes: usize,
    /// Taille du tampon mmap renvoyé par `v4l` (le tampon entier, pas la taille utile).
    pub buffer_len: usize,
    /// Trames sans `SOI` ou sans `EOI` dans la taille utile.
    pub incomplete_jpeg: u64,
    /// Trames sans table de Huffman.
    pub without_dht: u64,
    /// Trames avec bourrage d'octets nuls après `EOI`.
    pub padded: u64,
    /// Trames perdues par le pilote (trous dans les numéros de séquence).
    pub sequence_gaps: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete() -> JpegInfo {
        JpegInfo {
            has_soi: true,
            has_eoi: true,
            has_dht: true,
            trailing_padding: 0,
        }
    }

    #[test]
    fn empty_stats_have_no_rates() {
        let summary = CaptureStats::default().summary();
        assert_eq!(summary.frames, 0);
        assert_eq!(summary.fps, None);
        assert_eq!(summary.mean_bytes, None);
        assert_eq!(summary.min_bytes, None);
    }

    #[test]
    fn computes_fps_sizes_and_sequence_gaps() {
        let start = Instant::now();
        let mut stats = CaptureStats::default();
        // 16 trames espacées de 1/15 s : 15 intervalles en 1 s, soit 15 i/s.
        for i in 0..16u32 {
            let at = start + Duration::from_secs(1) * i / 15;
            // Séquences 0..=14 puis 16 : une trame perdue par le pilote.
            let sequence = if i == 15 { 16 } else { i };
            stats.record(at, 614_400, 40_000 + i as usize, sequence, complete());
        }
        let summary = stats.summary();
        assert_eq!(summary.frames, 16);
        let fps = summary.fps.expect("au moins deux trames");
        assert!((fps - 15.0).abs() < 0.01, "fps = {fps}");
        assert_eq!(summary.min_bytes, Some(40_000));
        assert_eq!(summary.max_bytes, 40_015);
        assert_eq!(summary.buffer_len, 614_400);
        assert_eq!(summary.sequence_gaps, 1);
        assert_eq!(summary.incomplete_jpeg, 0);
    }

    #[test]
    fn counts_incomplete_and_dht_less_frames() {
        let now = Instant::now();
        let mut stats = CaptureStats::default();
        stats.record(now, 10, 10, 0, JpegInfo::default());
        let no_dht = JpegInfo {
            has_dht: false,
            trailing_padding: 3,
            ..complete()
        };
        stats.record(now, 10, 10, 1, no_dht);
        let summary = stats.summary();
        assert_eq!(summary.incomplete_jpeg, 1);
        assert_eq!(summary.without_dht, 2);
        assert_eq!(summary.padded, 1);
        // Deux trames au même instant : cadence non calculable.
        assert_eq!(summary.fps, None);
    }
}
