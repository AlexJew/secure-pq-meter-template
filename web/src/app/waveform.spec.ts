import { SAMPLE_COUNT, synthesizeWaveform } from './waveform';

describe('synthesizeWaveform', () => {
  it('reduces the ideal wave to the fundamental peak at t=0', () => {
    const { ideal } = synthesizeWaveform([325.27, 0, 26.02], [0, 0, 0], 50, 40);
    expect(ideal[0]).toBeCloseTo(0, 5);
  });

  it('matches the fundamental peak a quarter cycle in when its phase is zero', () => {
    const frequencyHz = 50;
    const windowMs = 40;
    const { timesMs, ideal } = synthesizeWaveform([325.27], [0], frequencyHz, windowMs);
    const quarterCycleMs = 1000 / frequencyHz / 4;
    const index = timesMs.findIndex((t) => t >= quarterCycleMs);
    expect(ideal[index]).toBeCloseTo(325.27, 0);
  });

  it('sums every harmonic into the real wave, unlike the ideal (fundamental-only) wave', () => {
    const { ideal, real } = synthesizeWaveform([100, 0, 50], [0, 0, 0], 50, 40);
    // A non-zero 3rd harmonic makes the real wave diverge from the ideal
    // one somewhere in the window (they only coincide where sin(3*omega*t)
    // happens to be 0).
    const diverges = ideal.some((value, i) => Math.abs(value - real[i]) > 1e-6);
    expect(diverges).toBe(true);
  });

  it('returns SAMPLE_COUNT points spanning the requested window', () => {
    const { timesMs } = synthesizeWaveform([1], [0], 50, 40);
    expect(timesMs.length).toBe(SAMPLE_COUNT);
    expect(timesMs[0]).toBeCloseTo(0, 6);
    expect(timesMs[timesMs.length - 1]).toBeCloseTo(40, 6);
  });
});
