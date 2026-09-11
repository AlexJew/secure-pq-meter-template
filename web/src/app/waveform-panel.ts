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

  // Voltage on the left axis (V), current on the right (A) — same overlay
  // pattern as app.ts's combined active-power/power-factor chart.
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

  protected readonly options: ChartOptions<'line'> = {
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
        title: { display: true, text: 'A' },
        grid: { drawOnChartArea: false },
      },
    },
  };

  constructor() {
    setInterval(() => this.poll.update((value) => value + 1), POLL_INTERVAL_MS);
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
