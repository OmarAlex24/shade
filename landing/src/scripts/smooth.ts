/**
 * Smooth scroll with Lenis (window mode).
 *
 *   - Lenis v1.3.x in window mode (native scroll, position:sticky keeps working)
 *   - autoRaf: true → Lenis manages its own RAF loop
 *   - anchors: { offset: -NAV_HEIGHT } → Lenis intercepts hash links with the
 *     correct sticky-nav offset. No manual click handler (avoids double-fire).
 *   - prefers-reduced-motion: Lenis is NOT initialised; native scroll + CSS
 *     scroll-behavior:smooth take over.
 *
 * The old scroll-snap / overlap-stage layer stacking was removed with the
 * light redesign (no more sticky product bands), so there is no Snap setup here.
 */

import Lenis from 'lenis';
import { setLenis } from './lenis-instance';

const NAV_HEIGHT = 64;

function init(): void {
  // Bail on reduced motion — leave native scroll + scroll-padding intact
  if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) return;

  // Bail if Lenis is already running (HMR / double-init guard)
  if ((window as unknown as Record<string, unknown>).__lenis) return;

  const lenis = new Lenis({
    lerp: 0.1,
    smoothWheel: true,
    autoRaf: true,
    anchors: { offset: -NAV_HEIGHT },
  });

  (window as unknown as Record<string, unknown>).__lenis = lenis;

  // Publish to the shared registry so Nav (hide-on-scroll) can subscribe.
  setLenis(lenis);

  // Lenis owns smoothness; disable CSS scroll-behavior:smooth so they don't fight.
  // Set via JS so CSS keeps smooth as a no-JS / reduced-motion fallback.
  document.documentElement.style.scrollBehavior = 'auto';
}

if (typeof document !== 'undefined') {
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
}
