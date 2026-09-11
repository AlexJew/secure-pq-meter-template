import { Component, computed, input } from '@angular/core';
import type { ChartData, ChartOptions } from 'chart.js';
import { BaseChartDirective } from 'ng2-charts';
import { Reading } from './models/reading';

@Component({
  imports: [BaseChartDirective],
  selector: 'app-chart-panel',
  styleUrl: './chart-panel.scss',
  templateUrl: './chart-panel.html',
})
export class ChartPanel {
  readonly readings = input<Reading[]>([]);

  protected readonly data = computed<ChartData<'line', number[]>>(() => {
    const readings = this.readings();
    return {
      labels: readings.map((reading) => this.formatTime(reading.timestamp)),
      datasets: [
        {
          label: 'Active power (W)',
          data: readings.map((reading) => reading.active_power_l1_w),
          borderColor: '#2563eb',
          borderWidth: 2,
          pointRadius: 0,
          tension: 0.3,
          fill: false,
        },
      ],
    };
  });

  protected readonly options: ChartOptions<'line'> = {
    responsive: true,
    maintainAspectRatio: false,
    animation: false,
    plugins: { legend: { display: false } },
    scales: {
      x: {
        grid: { display: false },
        ticks: { maxTicksLimit: 8, maxRotation: 0 },
      },
      y: {
        title: { display: true, text: 'W' },
      },
    },
  };

  private formatTime(timestamp: string): string {
    return new Date(Number(timestamp)).toLocaleTimeString([], {
      hour12: false,
    });
  }
}
