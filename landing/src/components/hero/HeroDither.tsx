import { useEffect, useState } from 'react';
import { Dithering } from '@paper-design/shaders-react';

/**
 * HeroDither — client-only wrapper around the Paper Design <Dithering /> shader.
 *
 * Renders the hero's full-bleed dark "warp" band under the light copy: a navy
 * field with a dithered blue warp. It fills its parent (.hero-visual .shader is
 * position:absolute inset:0) and is purely decorative (pointer-events:none via
 * CSS).
 *
 * - prefers-reduced-motion: speed={0} → a single static frame (no rAF loop),
 *   keeping the look without motion.
 * - Rendered with `client:only="react"` from Hero.astro (WebGL can't be SSR'd),
 *   so `window` is always available — no hydration mismatch. If WebGL is
 *   unavailable the .hero-visual navy background shows as the fallback.
 */
const COLOR_BACK = '#0a0f1a';
const COLOR_FRONT = '#3b82f6';

const HeroDither = () => {
  const [reduced, setReduced] = useState(false);

  useEffect(() => {
    const mq = window.matchMedia('(prefers-reduced-motion: reduce)');
    setReduced(mq.matches);
    const onChange = (e: MediaQueryListEvent): void => setReduced(e.matches);
    mq.addEventListener('change', onChange);
    return () => mq.removeEventListener('change', onChange);
  }, []);

  return (
    <Dithering
      style={{ width: '100%', height: '100%' }}
      colorBack={COLOR_BACK}
      colorFront={COLOR_FRONT}
      shape="warp"
      type="4x4"
      size={1}
      speed={reduced ? 0 : 0.5}
      scale={0.9}
    />
  );
};

export default HeroDither;
