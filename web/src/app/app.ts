import { httpResource } from '@angular/common/http';
import { Component, signal } from '@angular/core';
import { ChartPanel, Series } from './chart-panel';
import { Reading } from './models/reading';
import { WaveformPanel } from './waveform-panel';

const POWER_SERIES: Series[] = [
  { label: 'Active power (W)', color: '#2563eb', unit: 'W', axis: 'left', value: (reading) => reading.active_power_l1_w },
  { label: 'Power factor', color: '#16a34a', unit: '', axis: 'right', value: (reading) => reading.power_factor_l1 },
];

// Matches pq-meter-server's web_api::READINGS_PATH; the dev proxy
// (proxy.conf.json) forwards it to the server's plain-HTTP data API.
const READINGS_PATH = '/edh/v1/readings';
// How often to re-poll for fresh readings. The server only has new data
// every api::PULL_INTERVAL (5s), so polling faster would just repeat rows.
const POLL_INTERVAL_MS = 5_000;
// Readings requested per poll; enough to fill the chart's visible window.
const READING_LIMIT = 120;

@Component({
  imports: [ChartPanel, WaveformPanel],
  selector: 'app-root',
  styleUrl: './app.scss',
  templateUrl: './app.html',
})
export class App {
  // Bumped on an interval so `readingsResource`'s URL function re-runs and
  // re-fetches, since httpResource otherwise only refetches when a signal it
  // read while computing the request changes.
  private readonly poll = signal(0);

  private readonly readingsResource = httpResource<Reading[]>(
    () => {
      this.poll();
      return `${READINGS_PATH}?limit=${READING_LIMIT}`;
    },
    { defaultValue: [] },
  );

  protected readonly readings = this.readingsResource.value;
  protected readonly powerSeries = POWER_SERIES;

  constructor() {
    setInterval(() => this.poll.update((value) => value + 1), POLL_INTERVAL_MS);
  }
}
