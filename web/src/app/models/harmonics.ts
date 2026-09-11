// Shape of a pq-meter-server GET /edh/v1/harmonics response — see
// crates/pq-meter-server/API.md and web_api.rs's `derive_harmonics`.
// Index i-1 of each array is harmonic i (i = 1..harmonic_count).
export interface Harmonics {
  frequency_hz: number;
  harmonic_count: number;
  window_ms: number;
  voltage: {
    a: number[];
    phi_deg: number[];
  };
  current: {
    b: number[];
    gamma_deg: number[];
  };
}
