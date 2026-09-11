import { Component, computed, input } from '@angular/core';
import type { ChartData, ChartOptions } from 'chart.js';
import { BaseChartDirective } from 'ng2-charts';
import { Reading } from './models/reading';

/** One line on the chart, plotted against either the left or right y-axis. */
export interface Series {
  label: string;
  color: string;
  /** Axis title; pass '' for a unitless series like power factor. */
  unit: string;
  axis: 'left' | 'right';
  value: (reading: Reading) => number;
}

const DEFAULT_SERIES: Series[] = [
  { label: 'Active power (W)', color: '#2563eb', unit: 'W', axis: 'left', value: (reading) => reading.active_power_l1_w },
];

@Component({
  imports: [BaseChartDirective],
  selector: 'app-chart-panel',
  styleUrl: './chart-panel.scss',
  templateUrl: './chart-panel.html',
})
export class ChartPanel {
  readonly readings = input<Reading[]>([]);
  readonly title = input('Active power · L1');
  readonly series = input<Series[]>(DEFAULT_SERIES);

  protected readonly data = computed<ChartData<'line', number[]>>(() => {
    const readings = this.readings();
    return {
      labels: readings.map((reading) => this.formatTime(reading.timestamp)),
      datasets: this.series().map((s) => ({
        label: s.label,
        data: readings.map(s.value),
        borderColor: s.color,
        borderWidth: 5,
        pointRadius: 0,
        tension: 0.3,
        fill: false,
        yAxisID: s.axis === 'left' ? 'y' : 'y1',
      })),
    };
  });

  protected readonly options = computed<ChartOptions<'line'>>(() => {
    const series = this.series();
    const left = series.find((s) => s.axis === 'left');
    const right = series.find((s) => s.axis === 'right');

    return {
      responsive: true,
      maintainAspectRatio: false,
      animation: false,
      plugins: { legend: { display: series.length > 1 } },
      scales: {
        x: {
          grid: { display: false },
          ticks: { maxTicksLimit: 8, maxRotation: 0 },
        },
        ...(left && {
          y: {
            type: 'linear',
            position: 'left',
            title: { display: left.unit.length > 0, text: left.unit },
          },
        }),
        ...(right && {
          y1: {
            type: 'linear',
            position: 'right',
            title: { display: right.unit.length > 0, text: right.unit },
            grid: { drawOnChartArea: false },
          },
        }),
      },
    };
  });

  private formatTime(timestamp: string): string {
    return new Date(Number(timestamp)).toLocaleTimeString([], {
      hour12: false,
    });
  }
}
