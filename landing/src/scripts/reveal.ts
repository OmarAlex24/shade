/**
 * Scroll-reveal utility using IntersectionObserver.
 * Elements with [data-reveal] animate in when they enter the viewport.
 * Supports stagger via data-reveal-delay (value in ms, e.g. data-reveal-delay="100").
 * Respects prefers-reduced-motion: reveals everything immediately when set.
 */

export function initReveal(): void {
  const prefersReducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  const elements = Array.from(document.querySelectorAll<HTMLElement>('[data-reveal]'));

  if (elements.length === 0) return;

  if (prefersReducedMotion) {
    // Reveal all immediately without animation
    elements.forEach((el) => {
      el.classList.add('is-revealed');
    });
    return;
  }

  const observer = new IntersectionObserver(
    (entries) => {
      entries.forEach((entry) => {
        if (entry.isIntersecting) {
          const el = entry.target as HTMLElement;
          const delay = parseInt(el.dataset.revealDelay ?? '0', 10);

          if (delay > 0) {
            setTimeout(() => {
              el.classList.add('is-revealed');
            }, delay);
          } else {
            el.classList.add('is-revealed');
          }

          // Unobserve after reveal (fire once)
          observer.unobserve(el);
        }
      });
    },
    {
      threshold: 0.1,
      rootMargin: '0px 0px -40px 0px',
    }
  );

  elements.forEach((el) => observer.observe(el));
}

// Auto-execute
if (typeof document !== 'undefined') {
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', initReveal);
  } else {
    initReveal();
  }
}
