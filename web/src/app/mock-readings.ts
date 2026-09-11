import { Reading } from './models/reading';

const BASE_POWER_W = 22;
const STEP_MS = 1000;

// Placeholder data until the pq-meter-server exposes a plain-HTTP data
// endpoint that the dev proxy can forward to.
export function generateMockReadings(count = 60): Reading[] {
  const now = Date.now();
  return Array.from({ length: count }, (_, i) => {
    const wobble = Math.sin(i / 6) * 1.5;
    const noise = (Math.random() - 0.5) * 0.8;
    return {
      id: i + 1,
      index: i + 1,
      timestamp: String(now - (count - 1 - i) * STEP_MS),
      voltage_l1_v: 237.4 + (Math.random() - 0.5) * 0.4,
      current_l1_a: 0.27 + (Math.random() - 0.5) * 0.01,
      active_power_l1_w: BASE_POWER_W + wobble + noise,
      reactive_power_l1_var: -35.3 + (Math.random() - 0.5),
      phase_angle_l1_deg: -58.75 + (Math.random() - 0.5) * 0.3,
    };
  });
}
