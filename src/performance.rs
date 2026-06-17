use std::fs;
use std::path::Path;
use std::time::Duration;

use color_eyre::eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::sync::{DetailFrame, DetailPayload, SyncTuning};

#[derive(Debug, Default, Serialize)]
pub struct PerformanceProfile {
    pub total_ms: u64,
    pub phases: Vec<PhaseProfile>,
    pub counters: ProfileCounters,
    pub sync_tuning: Option<SyncTuning>,
}

impl PerformanceProfile {
    pub fn record_phase(&mut self, name: &str, duration: Duration) {
        self.phases.push(PhaseProfile {
            name: name.to_string(),
            ms: duration_ms(duration),
        });
    }

    pub fn finish(&mut self, total: Duration) {
        self.total_ms = duration_ms(total);
    }

    pub fn write_json(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json)
            .wrap_err_with(|| format!("failed to write profile {}", path.display()))
    }

    pub fn print_human(&self) {
        println!("Performance profile:");
        println!("  total: {} ms", self.total_ms);
        if let Some(tuning) = self.sync_tuning {
            let tuning = tuning.normalized();
            println!(
                "  sync tuning: signature-window={}..{} bytes, detail-chunk={}, detail-batch-frames={}, detail-batch-payload={}",
                indicatif::HumanBytes(tuning.signature_window_min as u64),
                indicatif::HumanBytes(tuning.signature_window_max as u64),
                indicatif::HumanBytes(tuning.detail_chunk_bytes as u64),
                tuning.detail_batch_frames,
                indicatif::HumanBytes(tuning.detail_batch_payload_bytes as u64)
            );
        }
        println!("  phases:");
        for phase in &self.phases {
            println!("    {}: {} ms", phase.name, phase.ms);
        }
        println!(
            "  counts: local-entries={}, local-changes={}, remote-changes={}, active-actions={}, unresolved-conflicts={}",
            self.counters.local_entries,
            self.counters.local_changes,
            self.counters.remote_changes,
            self.counters.active_actions,
            self.counters.unresolved_conflicts
        );
        println!(
            "  changed bytes: local={}, remote={}",
            indicatif::HumanBytes(self.counters.local_changed_bytes),
            indicatif::HumanBytes(self.counters.remote_changed_bytes)
        );
        println!(
            "  signatures: local={}, remote={}",
            self.counters.local_signatures, self.counters.remote_signatures
        );
        if self.counters.streamed_details {
            print_transfer("remote->local", &self.counters.streaming.remote_to_local);
            print_transfer("local->remote", &self.counters.streaming.local_to_remote);
            if let Some(remote_server) = &self.counters.streaming.remote_server {
                println!(
                    "  remote server stream: detail-generate={} ms apply-frames={} ms apply-finish={} ms detail-batches={} apply-batches={}",
                    remote_server.detail_generate_ms,
                    remote_server.apply_frames_ms,
                    remote_server.apply_finish_ms,
                    remote_server.detail_batches,
                    remote_server.apply_batches
                );
                print_transfer("remote-server detail", &remote_server.detail_transfer);
                print_transfer("remote-server apply", &remote_server.apply_transfer);
            }
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct PhaseProfile {
    pub name: String,
    pub ms: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct ProfileCounters {
    pub local_entries: usize,
    pub local_changes: usize,
    pub remote_changes: usize,
    pub local_changed_bytes: u64,
    pub remote_changed_bytes: u64,
    pub total_actions: usize,
    pub active_actions: usize,
    pub unresolved_conflicts: usize,
    pub identical_actions: usize,
    pub local_signatures: usize,
    pub remote_signatures: usize,
    pub streamed_details: bool,
    pub streaming: StreamingProfile,
}

#[derive(Debug, Default, Serialize)]
pub struct StreamingProfile {
    pub remote_to_local: DetailTransferStats,
    pub local_to_remote: DetailTransferStats,
    pub remote_server: Option<RemoteStreamProfile>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RemoteStreamProfile {
    pub detail_generate_ms: u64,
    pub detail_batches: u64,
    pub apply_frames_ms: u64,
    pub apply_finish_ms: u64,
    pub apply_batches: u64,
    pub detail_transfer: DetailTransferStats,
    pub apply_transfer: DetailTransferStats,
}

impl RemoteStreamProfile {
    pub fn is_empty(&self) -> bool {
        self.detail_batches == 0 && self.apply_batches == 0 && self.apply_finish_ms == 0
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DetailTransferStats {
    pub batches: u64,
    pub empty_batches: u64,
    pub frames: u64,
    pub message_payload_bytes: u64,
    pub reconstructed_bytes: u64,
    pub file_bytes: u64,
    pub diff_literal_bytes: u64,
    pub diff_copy_bytes: u64,
    pub file_byte_frames: u64,
    pub diff_literal_frames: u64,
    pub diff_copy_frames: u64,
    pub max_batch_frames: u64,
    pub max_batch_payload_bytes: u64,
}

impl DetailTransferStats {
    pub fn record_batch(&mut self, frames: &[DetailFrame]) {
        self.batches += 1;
        if frames.is_empty() {
            self.empty_batches += 1;
            return;
        }

        self.frames += frames.len() as u64;
        self.max_batch_frames = self.max_batch_frames.max(frames.len() as u64);
        let mut batch_payload_bytes = 0;

        for frame in frames {
            match &frame.payload {
                DetailPayload::FileBytes(bytes) => {
                    let len = bytes.len() as u64;
                    self.file_byte_frames += 1;
                    self.file_bytes += len;
                    self.message_payload_bytes += len;
                    self.reconstructed_bytes += len;
                    batch_payload_bytes += len;
                }
                DetailPayload::DiffBytes(bytes) => {
                    let len = bytes.len() as u64;
                    self.diff_literal_frames += 1;
                    self.diff_literal_bytes += len;
                    self.message_payload_bytes += len;
                    self.reconstructed_bytes += len;
                    batch_payload_bytes += len;
                }
                DetailPayload::DiffCopy { len, .. } => {
                    self.diff_copy_frames += 1;
                    self.diff_copy_bytes += *len;
                    self.reconstructed_bytes += *len;
                }
                DetailPayload::FileBegin
                | DetailPayload::FileEnd
                | DetailPayload::DiffBegin
                | DetailPayload::DiffEnd => {}
            }
        }

        self.max_batch_payload_bytes = self.max_batch_payload_bytes.max(batch_payload_bytes);
    }
}

pub fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn print_transfer(label: &str, stats: &DetailTransferStats) {
    println!(
        "  stream {}: batches={} empty-batches={} frames={} payload={} reconstructed={} file-bytes={} diff-literal={} diff-copy={} max-batch-frames={} max-batch-payload={}",
        label,
        stats.batches,
        stats.empty_batches,
        stats.frames,
        indicatif::HumanBytes(stats.message_payload_bytes),
        indicatif::HumanBytes(stats.reconstructed_bytes),
        indicatif::HumanBytes(stats.file_bytes),
        indicatif::HumanBytes(stats.diff_literal_bytes),
        indicatif::HumanBytes(stats.diff_copy_bytes),
        stats.max_batch_frames,
        indicatif::HumanBytes(stats.max_batch_payload_bytes)
    );
}
