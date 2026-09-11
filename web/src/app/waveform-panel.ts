import { httpResource } from '@angular/common/http';
import { afterNextRender, Component, computed, ElementRef, input, signal, viewChild } from '@angular/core';
import type { ChartData, ChartOptions } from 'chart.js';
import { BaseChartDirective } from 'ng2-charts';
import { Harmonics } from './models/harmonics';
import { synthesizeWaveform } from './waveform';

// Matches pq-meter-server's web_api::HARMONICS_PATH; the dev proxy
// (proxy.conf.json) forwards it to the server's plain-HTTP data API.
const HARMONICS_PATH = '/edh/v1/harmonics';
// The server only has a fresh measurement every api::PULL_INTERVAL (5s), so
// polling faster would just re-synthesize the same spectrum.
const POLL_INTERVAL_MS = 5_000;

// Selectable full-scale values (A) for the current axis slider, smallest
// (most zoomed in) first. 0.01/0.1/0.5 are the anchors; the values between
// them give finer control than jumping straight between those three.
const CURRENT_SCALES_A = [0.01, 0.02, 0.05, 0.1, 0.2, 0.3, 0.5, 1, 2, 5];

@Component({
  imports: [BaseChartDirective],
  selector: 'app-waveform-panel',
  styleUrl: './waveform-panel.scss',
  templateUrl: './waveform-panel.html',
})
export class WaveformPanel {
  /** Latest power_factor_l1, for the lamp; undefined before the first reading. */
  readonly powerFactor = input<number | undefined>(undefined);

  // Bumped on an interval so `harmonicsResource`'s URL function re-runs and
  // re-fetches — see app.ts's identical pattern for why.
  private readonly poll = signal(0);

  private readonly harmonicsResource = httpResource<Harmonics | null>(
    () => {
      this.poll();
      return HARMONICS_PATH;
    },
    { defaultValue: null },
  );

  protected readonly harmonics = this.harmonicsResource.value;

  /** Index into CURRENT_SCALES_A the slider currently selects. */
  protected readonly currentScaleIndex = signal(CURRENT_SCALES_A.indexOf(0.1));
  protected readonly currentScaleA = computed(() => CURRENT_SCALES_A[this.currentScaleIndex()]);
  protected readonly currentScaleMax = CURRENT_SCALES_A.length - 1;

  protected readonly lampColor = computed(() => lampColorFor(this.powerFactor()));

  // Measured so the rotated slider (see waveform-panel.scss) can be sized to
  // exactly fill its wrapper's height — a CSS-only fixed length would either
  // fall short of, or overflow, the sidebar's actual (layout-dependent)
  // height.
  private readonly sliderWrap = viewChild<ElementRef<HTMLDivElement>>('sliderWrap');
  protected readonly sliderLengthPx = signal(160);

  // Voltage on the left axis (V), current on the right (A, scaled by the
  // slider) — same overlay pattern as app.ts's combined chart used to.
  protected readonly data = computed<ChartData<'line', number[]>>(() => {
    const h = this.harmonics();
    if (!h) {
      return { labels: [], datasets: [] };
    }

    const voltage = synthesizeWaveform(h.voltage.a, h.voltage.phi_deg, h.frequency_hz, h.window_ms);
    const current = synthesizeWaveform(h.current.b, h.current.gamma_deg, h.frequency_hz, h.window_ms);

    return {
      labels: voltage.timesMs.map((t) => t.toFixed(1)),
      datasets: [
        line('Voltage (ideal)', voltage.ideal, '#93c5fd', 'y', true),
        line('Voltage (real)', voltage.real, '#2563eb', 'y', false),
        line('Current (ideal)', current.ideal, '#fdba74', 'y1', true),
        line('Current (real)', current.real, '#d97706', 'y1', false),
      ],
    };
  });

  protected readonly options = computed<ChartOptions<'line'>>(() => {
    const scale = this.currentScaleA();
    return {
      responsive: true,
      maintainAspectRatio: false,
      animation: false,
      plugins: { legend: { display: true, labels: { boxWidth: 12 } } },
      scales: {
        x: {
          grid: { display: false },
          title: { display: true, text: 'ms' },
          ticks: { maxTicksLimit: 8, maxRotation: 0 },
        },
        y: {
          type: 'linear',
          position: 'left',
          title: { display: true, text: 'V' },
        },
        y1: {
          type: 'linear',
          position: 'right',
          min: -scale,
          max: scale,
          title: { display: true, text: 'A' },
          grid: { drawOnChartArea: false },
        },
      },
    };
  });

  constructor() {
    setInterval(() => this.poll.update((value) => value + 1), POLL_INTERVAL_MS);

    afterNextRender(() => {
      const el = this.sliderWrap()?.nativeElement;
      if (!el) {
        return;
      }
      const update = () => this.sliderLengthPx.set(el.clientHeight);
      update();
      new ResizeObserver(update).observe(el);
    });
  }

  protected onCurrentScaleInput(event: Event): void {
    this.currentScaleIndex.set(Number((event.target as HTMLInputElement).value));
  }
}

function line(label: string, data: number[], color: string, axis: 'y' | 'y1', ideal: boolean) {
  return {
    label,
    data,
    borderColor: color,
    borderDash: ideal ? [4, 4] : undefined,
    borderWidth: ideal ? 3 : 5,
    pointRadius: 0,
    tension: 0,
    fill: false,
    yAxisID: axis,
  };
}

/**
 * Green (power factor 1, good) to red (power factor 0, bad); gray when
 * unknown. Squared rather than linear: real-world power factor quality is
 * judged against thresholds well above 0 (utilities often flag anything
 * under ~0.9 as poor), so a linear ramp left a "really bad" 0.3-ish reading
 * looking merely yellow-orange instead of red — squaring pulls the whole
 * mid-range down toward red and only lets the top of the range read green.
 */
function lampColorFor(powerFactor: number | undefined): string {
  if (powerFactor === undefined) {
    return '#9ca3af';
  }
  const clamped = Math.min(1, Math.max(0, Math.abs(powerFactor)));
  const hue = clamped ** 2 * 120;
  return `hsl(${hue}, 80%, 45%)`;
}
