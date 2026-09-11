import { httpResource } from '@angular/common/http';
import { Component, computed, signal } from '@angular/core';
import { Reading } from './models/reading';
import { WaveformPanel } from './waveform-panel';

// Matches pq-meter-server's web_api::READINGS_PATH; the dev proxy
// (proxy.conf.json) forwards it to the server's plain-HTTP data API.
const READINGS_PATH = '/edh/v1/readings';
// How often to re-poll for fresh readings. The server only has new data
// every api::PULL_INTERVAL (5s), so polling faster would just repeat rows.
const POLL_INTERVAL_MS = 5_000;
// Readings requested per poll — only the latest one is used (for the power
// factor lamp), but a small window is cheap and gives room to grow.
const READING_LIMIT = 20;

@Component({
  imports: [WaveformPanel],
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

  // `readings()` is answered oldest-first (see ServerStore::readings), so
  // the most recent one is the last element.
  protected readonly latestPowerFactor = computed<number | undefined>(() => {
    const readings = this.readingsResource.value();
    return readings.length > 0 ? readings[readings.length - 1].power_factor_l1 : undefined;
  });

  constructor() {
    setInterval(() => this.poll.update((value) => value + 1), POLL_INTERVAL_MS);
  }
}
