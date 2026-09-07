/**
 * <pixel-canvas> — a hover-reactive grid of pixels that shimmers in. Drop one
 * inside any positioned, hoverable container:
 *
 *   <div class="cell">
 *     <pixel-canvas data-colors="#dbe7ff,#bdd0fb,#2563eb" data-gap="6" data-speed="35"></pixel-canvas>
 *     ...content...
 *   </div>
 *
 * The canvas fills its parent (pointer-events:none, behind content). On parent hover
 * the pixels grow in from the center outward and shimmer; on leave they shrink away.
 * The rAF loop runs ONLY while animating — idle cards cost nothing. Disabled entirely
 * under prefers-reduced-motion (no listeners attached, no canvas motion).
 *
 * Attributes:
 *   data-colors  comma-separated hex colors (a random one per pixel)
 *   data-gap     spacing between pixels in px (4–50, default 5)
 *   data-speed   shimmer speed 0–100 (default 35)
 *   data-no-focus  if present, focus/blur on the parent does not trigger the effect
 *   data-static  if present, the pixels shimmer continuously (ambient) instead of on
 *                hover — gated to on-screen via IntersectionObserver so it costs
 *                nothing while scrolled away. Pair with a low data-speed + CSS opacity
 *                for a calm backdrop.
 */

type AnimationName = 'appear' | 'disappear';

class Pixel {
  private readonly ctx: CanvasRenderingContext2D;
  private readonly x: number;
  private readonly y: number;
  private readonly color: string;
  private readonly speed: number;
  private readonly delay: number;
  private readonly sizeStep: number;
  private readonly counterStep: number;
  private readonly minSize = 0.5;
  private readonly maxSizeInteger = 2;
  private readonly maxSize: number;
  private size = 0;
  private counter = 0;
  private isReverse = false;
  private isShimmer = false;
  isIdle = false;

  constructor(
    canvas: HTMLCanvasElement,
    ctx: CanvasRenderingContext2D,
    x: number,
    y: number,
    color: string,
    speed: number,
    delay: number,
  ) {
    this.ctx = ctx;
    this.x = x;
    this.y = y;
    this.color = color;
    this.speed = this.random(0.1, 0.9) * speed;
    this.delay = delay;
    this.sizeStep = Math.random() * 0.4;
    this.maxSize = this.random(this.minSize, this.maxSizeInteger);
    this.counterStep = Math.random() * 4 + (canvas.width + canvas.height) * 0.01;
  }

  private random(min: number, max: number): number {
    return Math.random() * (max - min) + min;
  }

  private draw(): void {
    const centerOffset = this.maxSizeInteger * 0.5 - this.size * 0.5;
    this.ctx.fillStyle = this.color;
    this.ctx.fillRect(this.x + centerOffset, this.y + centerOffset, this.size, this.size);
  }

  appear(): void {
    this.isIdle = false;
    if (this.counter <= this.delay) {
      this.counter += this.counterStep;
      return;
    }
    if (this.size >= this.maxSize) this.isShimmer = true;
    if (this.isShimmer) this.shimmer();
    else this.size += this.sizeStep;
    this.draw();
  }

  disappear(): void {
    this.isShimmer = false;
    this.counter = 0;
    if (this.size <= 0) {
      this.isIdle = true;
      return;
    }
    this.size -= 0.1;
    this.draw();
  }

  private shimmer(): void {
    if (this.size >= this.maxSize) this.isReverse = true;
    else if (this.size <= this.minSize) this.isReverse = false;
    if (this.isReverse) this.size -= this.speed;
    else this.size += this.speed;
    this.draw();
  }
}

class PixelCanvas extends HTMLElement {
  private readonly canvas: HTMLCanvasElement;
  private readonly ctx: CanvasRenderingContext2D | null;
  private readonly reducedMotion: boolean;
  private pixels: Pixel[] = [];
  private animationFrame = 0;
  private readonly timeInterval = 1000 / 60;
  private timePrevious = 0;
  private resizeObserver?: ResizeObserver;
  private intersectionObserver?: IntersectionObserver;
  private host: HTMLElement | null = null;
  private readonly onEnter = (): void => this.handleAnimation('appear');
  private readonly onLeave = (): void => this.handleAnimation('disappear');

  static register(tag = 'pixel-canvas'): void {
    if (typeof window !== 'undefined' && 'customElements' in window && !customElements.get(tag)) {
      customElements.define(tag, PixelCanvas);
    }
  }

  private static readonly css = `
    :host { display:grid; inline-size:100%; block-size:100%; overflow:hidden; }
    canvas { display:block; inline-size:100%; block-size:100%; }
  `;

  constructor() {
    super();
    this.reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    const shadow = this.attachShadow({ mode: 'open' });
    const style = document.createElement('style');
    style.textContent = PixelCanvas.css;
    this.canvas = document.createElement('canvas');
    shadow.append(style, this.canvas);
    this.ctx = this.canvas.getContext('2d');
  }

  private get colors(): string[] {
    return (this.dataset.colors ?? '#f8fafc,#f1f5f9,#cbd5e1').split(',').map((c) => c.trim());
  }
  private get gap(): number {
    const v = parseInt(this.dataset.gap ?? '5', 10);
    return Math.max(4, Math.min(50, Number.isFinite(v) ? v : 5));
  }
  private get speed(): number {
    const v = parseInt(this.dataset.speed ?? '35', 10);
    const clamped = Math.max(0, Math.min(100, Number.isFinite(v) ? v : 35));
    return clamped * 0.001;
  }
  private get noFocus(): boolean {
    return this.hasAttribute('data-no-focus');
  }
  private get isStatic(): boolean {
    return this.hasAttribute('data-static');
  }

  connectedCallback(): void {
    if (this.reducedMotion || !this.ctx) return;
    this.host = this.parentElement;
    if (!this.host) return;

    this.resizeObserver = new ResizeObserver(() => this.init());
    this.resizeObserver.observe(this);

    if (this.isStatic) {
      // Ambient mode: shimmer continuously, but only while on-screen.
      this.intersectionObserver = new IntersectionObserver((entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) this.handleAnimation('appear');
          else cancelAnimationFrame(this.animationFrame);
        }
      });
      this.intersectionObserver.observe(this);
    } else {
      this.host.addEventListener('pointerenter', this.onEnter);
      this.host.addEventListener('pointerleave', this.onLeave);
      if (!this.noFocus) {
        this.host.addEventListener('focusin', this.onEnter);
        this.host.addEventListener('focusout', this.onLeave);
      }
    }

    this.init();
  }

  disconnectedCallback(): void {
    this.resizeObserver?.disconnect();
    this.intersectionObserver?.disconnect();
    cancelAnimationFrame(this.animationFrame);
    this.host?.removeEventListener('pointerenter', this.onEnter);
    this.host?.removeEventListener('pointerleave', this.onLeave);
    this.host?.removeEventListener('focusin', this.onEnter);
    this.host?.removeEventListener('focusout', this.onLeave);
  }

  private init(): void {
    const rect = this.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return;
    this.canvas.width = Math.ceil(rect.width);
    this.canvas.height = Math.ceil(rect.height);
    this.createPixels();
  }

  private createPixels(): void {
    if (!this.ctx) return;
    const { colors, gap } = this;
    const cx = this.canvas.width / 2;
    const cy = this.canvas.height / 2;
    this.pixels = [];
    for (let x = 0; x < this.canvas.width; x += gap) {
      for (let y = 0; y < this.canvas.height; y += gap) {
        const color = colors[Math.floor(Math.random() * colors.length)];
        const dx = x - cx;
        const dy = y - cy;
        const delay = Math.sqrt(dx * dx + dy * dy);
        this.pixels.push(new Pixel(this.canvas, this.ctx, x, y, color, this.speed, delay));
      }
    }
  }

  private handleAnimation(name: AnimationName): void {
    cancelAnimationFrame(this.animationFrame);
    this.animationFrame = requestAnimationFrame((t) => this.runFrame(name, t));
  }

  private runFrame(name: AnimationName, now: number): void {
    this.animationFrame = requestAnimationFrame((t) => this.runFrame(name, t));
    const passed = now - this.timePrevious;
    if (passed < this.timeInterval) return;
    this.timePrevious = now - (passed % this.timeInterval);

    if (!this.ctx) return;
    this.ctx.clearRect(0, 0, this.canvas.width, this.canvas.height);

    let allIdle = true;
    for (const pixel of this.pixels) {
      pixel[name]();
      if (!pixel.isIdle) allIdle = false;
    }
    if (allIdle) cancelAnimationFrame(this.animationFrame);
  }
}

PixelCanvas.register();
