import { httpResource } from '@angular/common/http';
import { Component, computed, signal } from '@angular/core';
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

@Component({
  imports: [BaseChartDirective],
  selector: 'app-waveform-panel',
  styleUrl: './waveform-panel.scss',
  templateUrl: './waveform-panel.html',
})
export class WaveformPanel {
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

  protected readonly voltageData = computed<ChartData<'line', number[]>>(() =>
    this.waveformData(this.harmonics()?.voltage.a, this.harmonics()?.voltage.phi_deg, 'Voltage', '#2563eb'),
  );
  protected readonly currentData = computed<ChartData<'line', number[]>>(() =>
    this.waveformData(this.harmonics()?.current.b, this.harmonics()?.current.gamma_deg, 'Current', '#d97706'),
  );

  protected readonly voltageOptions = this.axisOptions('V');
  protected readonly currentOptions = this.axisOptions('A');

  constructor() {
    setInterval(() => this.poll.update((value) => value + 1), POLL_INTERVAL_MS);
  }

  private waveformData(
    peaks: number[] | undefined,
    phasesDeg: number[] | undefined,
    label: string,
    color: string,
  ): ChartData<'line', number[]> {
    const harmonics = this.harmonics();
    if (!peaks || !phasesDeg || !harmonics) {
      return { labels: [], datasets: [] };
    }

    const { timesMs, ideal, real } = synthesizeWaveform(peaks, phasesDeg, harmonics.frequency_hz, harmonics.window_ms);
    return {
      labels: timesMs.map((t) => t.toFixed(1)),
      datasets: [
        {
          label: `${label} (ideal)`,
          data: ideal,
          borderColor: '#9ca3af',
          borderDash: [4, 4],
          borderWidth: 3,
          pointRadius: 0,
          tension: 0,
          fill: false,
        },
        {
          label: `${label} (real)`,
          data: real,
          borderColor: color,
          borderWidth: 5,
          pointRadius: 0,
          tension: 0,
          fill: false,
        },
      ],
    };
  }

  private axisOptions(unit: string): ChartOptions<'line'> {
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
          title: { display: true, text: unit },
        },
      },
    };
  }
}
