import { writable } from 'svelte/store';

export interface AmbientColor {
  r: number;
  g: number;
  b: number;
}

export const ambientColor = writable<AmbientColor | null>(null);
