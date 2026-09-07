/**
 * Animated SVG graph driver.
 *
 * Containers marked `.js-graph` get `.is-drawn` added when they scroll into view.
 * That class triggers the CSS in global.css:
 *   - .g-node / .g-core / .g-label pop / fade in
 *   - .g-flow pulses travel along connections (pure CSS, infinite)
 *   - .g-pulse rings expand around cores (pure CSS, infinite)
 *
 * This script only measures `.g-line` paths so their draw-in (stroke-dashoffset
 * 1 → 0) scales to each path's true length. It deliberately does NOT touch
 * `.g-flow` (its dasharray is a fixed travelling-dash pattern set in CSS).
 *
 * Respects prefers-reduced-motion: marks every graph drawn immediately,
 * the reduced-motion CSS block then shows the static final state.
 */

function drawGraph(graph: HTMLElement): void {
  const lines = graph.querySelectorAll<SVGGeometryElement>('.g-line');

  lines.forEach((line) => {
    let len = 600;
    try {
      len = Math.ceil(line.getTotalLength());
    } catch {
      // getTotalLength unsupported on this element — keep fallback length
    }
    line.style.strokeDasharray = String(len);
    line.style.strokeDashoffset = String(len);

    // Two rAFs so the initial offset is committed before the transition runs
    requestAnimationFrame(() => {
      requestAnimationFrame(() => {
        line.style.strokeDashoffset = '0';
      });
    });
  });

  graph.classList.add('is-drawn');
}

export function initGraphs(): void {
  const graphs = Array.from(document.querySelectorAll<HTMLElement>('.js-graph'));
  if (graphs.length === 0) return;

  const prefersReducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  if (prefersReducedMotion || !('IntersectionObserver' in window)) {
    // No motion (or no IO): show the final drawn state immediately.
    graphs.forEach((g) => g.classList.add('is-drawn'));
    return;
  }

  const observer = new IntersectionObserver(
    (entries) => {
      entries.forEach((entry) => {
        if (!entry.isIntersecting) return;
        drawGraph(entry.target as HTMLElement);
        observer.unobserve(entry.target);
      });
    },
    { threshold: 0.2 }
  );

  graphs.forEach((g) => observer.observe(g));
}

if (typeof document !== 'undefined') {
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', initGraphs);
  } else {
    initGraphs();
  }
}
