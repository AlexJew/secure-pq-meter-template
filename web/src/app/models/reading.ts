// Shape of one meter reading, mirroring the entries pq-meter-server writes
// to data.json.
export interface Reading {
  id: number;
  index: number;
  timestamp: string;
  voltage_l1_v: number;
  current_l1_a: number;
  active_power_l1_w: number;
  reactive_power_l1_var: number;
  phase_angle_l1_deg: number;
}
