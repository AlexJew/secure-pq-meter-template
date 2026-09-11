import { Component, signal } from '@angular/core';
import { ChartPanel } from './chart-panel';
import { generateMockReadings } from './mock-readings';
import { Reading } from './models/reading';

@Component({
  imports: [ChartPanel],
  selector: 'app-root',
  styleUrl: './app.scss',
  templateUrl: './app.html',
})
export class App {
  // Mock data for now; swap in an HttpClient-based service once the
  // pq-meter-server exposes a plain-HTTP data endpoint behind the dev proxy.
  protected readonly readings = signal<Reading[]>(generateMockReadings());
}
