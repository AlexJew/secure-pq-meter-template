// Synthesizes an ideal (fundamental-only) and a real (all-harmonics) sine
// waveform from a peak/phase harmonic spectrum, per API.md's formulas:
//   ideal(t) = peaks[0] * sin(2*pi*f*t + phasesDeg[0])
//   real(t)  = sum_i peaks[i] * sin(2*pi*(i+1)*f*t + phasesDeg[i])
// (voltage's phasesDeg[0] is always ~0, the phase reference, so `ideal`
// reduces to API.md's `a1 * sin(2*pi*f*t)` for it; current's carries its
// own fundamental phase, matching `b1 * sin(2*pi*f*t + gamma1)`.)

export const SAMPLE_COUNT = 200;

export interface Waveform {
  /** Time of each sample, in milliseconds from the start of the window. */
  timesMs: number[];
  ideal: number[];
  real: number[];
}

export function synthesizeWaveform(
  peaks: number[],
  phasesDeg: number[],
  frequencyHz: number,
  windowMs: number,
): Waveform {
  const timesMs: number[] = [];
  const ideal: number[] = [];
  const real: number[] = [];
  const windowS = windowMs / 1000;

  for (let sample = 0; sample < SAMPLE_COUNT; sample++) {
    const tS = (sample / (SAMPLE_COUNT - 1)) * windowS;
    const omegaT = 2 * Math.PI * frequencyHz * tS;

    timesMs.push(tS * 1000);
    ideal.push((peaks[0] ?? 0) * Math.sin(omegaT + toRad(phasesDeg[0])));

    let sum = 0;
    for (let i = 0; i < peaks.length; i++) {
      const order = i + 1;
      sum += peaks[i] * Math.sin(order * omegaT + toRad(phasesDeg[i]));
    }
    real.push(sum);
  }

  return { timesMs, ideal, real };
}

function toRad(deg: number | undefined): number {
  return ((deg ?? 0) * Math.PI) / 180;
}
