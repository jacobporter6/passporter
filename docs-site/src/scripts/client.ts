// Client runtime: Mermaid rendering (theme-aware), diagram zoom, checklists.

const css = (name: string) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();

function mermaidTheme() {
  const dark = document.documentElement.dataset.theme !== 'light';
  return {
    theme: 'base' as const,
    themeVariables: {
      darkMode: dark,
      fontFamily: css('--sl-font'),
      fontSize: '15px',
      // Mermaid 12's "neo" look: a blue-to-purple gradient stroke on nodes and a
      // soft shadow tuned per theme (its default is a grey glow that reads as
      // a smudge on Tokyo Night).
      useGradient: true,
      gradientStart: css('--pp-blue'),
      gradientStop: css('--pp-purple'),
      dropShadow: dark ? 'drop-shadow(0 4px 10px rgba(0,0,0,0.45))' : 'drop-shadow(0 2px 4px rgba(30,40,80,0.10))',
      strokeWidth: 1.2,
      background: css('--pp-bg-deep'),
      primaryColor: css('--pp-surface-2'),
      primaryBorderColor: css('--pp-blue'),
      primaryTextColor: css('--pp-fg'),
      secondaryColor: css('--pp-surface'),
      secondaryBorderColor: css('--pp-purple'),
      secondaryTextColor: css('--pp-fg'),
      tertiaryColor: css('--pp-bg'),
      tertiaryBorderColor: css('--pp-border'),
      tertiaryTextColor: css('--pp-fg'),
      lineColor: css('--pp-comment'),
      textColor: css('--pp-fg'),
      mainBkg: css('--pp-surface-2'),
      nodeBorder: css('--pp-blue'),
      clusterBkg: css('--pp-surface'),
      clusterBorder: css('--pp-border'),
      titleColor: css('--pp-fg'),
      edgeLabelBackground: css('--pp-surface'),
      actorBkg: css('--pp-surface-2'),
      actorBorder: css('--pp-blue'),
      actorTextColor: css('--pp-fg'),
      actorLineColor: css('--pp-comment'),
      signalColor: css('--pp-fg-muted'),
      signalTextColor: css('--pp-fg'),
      labelBoxBkgColor: css('--pp-surface'),
      labelBoxBorderColor: css('--pp-border'),
      labelTextColor: css('--pp-fg'),
      loopTextColor: css('--pp-fg-muted'),
      noteBkgColor: css('--pp-surface'),
      noteBorderColor: css('--pp-yellow'),
      noteTextColor: css('--pp-fg'),
      activationBkgColor: css('--pp-surface'),
      activationBorderColor: css('--pp-cyan'),
      sequenceNumberColor: css('--pp-bg-deep'),
      git0: css('--pp-blue'), git1: css('--pp-purple'), git2: css('--pp-green'), git3: css('--pp-orange'),
      pie1: css('--pp-blue'), pie2: css('--pp-purple'), pie3: css('--pp-teal'), pie4: css('--pp-orange'), pie5: css('--pp-red'), pie6: css('--pp-yellow'),
      cScale0: css('--pp-blue'), cScale1: css('--pp-purple'), cScale2: css('--pp-teal'), cScale3: css('--pp-orange'),
      // ER diagrams: quiet alternating rows (Mermaid 12 names them rowOdd/rowEven).
      rowOdd: css('--pp-surface'),
      rowEven: css('--pp-surface-2'),
      attributeBackgroundColorOdd: css('--pp-surface'),
      attributeBackgroundColorEven: css('--pp-surface-2'),
    },
  };
}

// Mermaid fetches one module per diagram type on first use. If those URLs went
// stale while this tab was open (the dev server re-optimised its dependencies,
// or a new release replaced the hashed bundles), the fetch fails. Reload once to
// get current URLs; a second failure within a minute is shown, not looped.
const STALE = /dynamically imported module|Importing a module script failed|error loading dynamically imported module|Outdated Optimize Dep/i;
const isStaleModule = (e: unknown) => STALE.test(String((e as Error)?.message ?? e));
function reloadOnce(): boolean {
  const key = 'pp-docs-stale-reload';
  let last = 0;
  try { last = Number(sessionStorage.getItem(key) ?? 0); } catch { /* storage blocked */ }
  if (Date.now() - last < 60_000) return false;
  try { sessionStorage.setItem(key, String(Date.now())); } catch { return false; }
  location.reload();
  return true;
}

let renderSeq = 0;
async function renderMermaid() {
  const blocks = [...document.querySelectorAll<HTMLPreElement>('pre.mermaid')];
  if (!blocks.length) return;
  let mermaid: typeof import('mermaid').default;
  try {
    ({ default: mermaid } = await import('mermaid'));
  } catch (e) {
    if (isStaleModule(e) && reloadOnce()) return;
    throw e;
  }
  mermaid.initialize({ startOnLoad: false, securityLevel: 'loose', ...mermaidTheme(), flowchart: { curve: 'basis', padding: 12, rankSpacing: 40, nodeSpacing: 34, minNodeWidth: 90 }, sequence: { mirrorActors: false, showSequenceNumbers: true, wrap: true, width: 110, actorMargin: 30, messageMargin: 30 } });
  const seq = ++renderSeq;
  for (const pre of blocks) {
    pre.dataset.source ??= pre.textContent ?? '';
    try {
      const { svg } = await mermaid.render(`m${seq}-${Math.random().toString(36).slice(2)}`, pre.dataset.source);
      if (seq !== renderSeq) return;
      pre.innerHTML = svg;
      keepReadable(pre);
      pre.setAttribute('data-processed', 'true');
      pre.removeAttribute('data-error');
    } catch (e) {
      if (isStaleModule(e) && reloadOnce()) return;
      pre.setAttribute('data-processed', 'true');
      pre.setAttribute('data-error', String((e as Error).message ?? e));
      pre.innerHTML = `<div class="diagram-error">Diagram failed to render:\n${String((e as Error).message ?? e).replace(/</g, '&lt;')}</div>`;
    }
  }
  attachExpanders();
  markScrollable();
}

// Mermaid sizes its SVG with max-width:100%, so a wide diagram shrinks until its
// 15px labels are 5px. Hold every diagram at no less than 80% of its natural
// size on a desktop (labels stay at 12px or more); a wider one scrolls inside
// its figure. Phones get a lower floor so the shape still shows; a tap opens
// the diagram full screen with pan and zoom. A diagram that misses fitting by
// 6% or less shrinks to fit instead (labels about 11px): a scrollbar that hides
// a sliver of the right edge costs more than a slightly smaller label.
function keepReadable(pre: HTMLElement) {
  const el = pre.querySelector('svg');
  const w = el?.viewBox?.baseVal?.width;
  if (!el || !w) return;
  const minScale = matchMedia('(max-width: 50rem)').matches ? 0.55 : 0.8;
  const fit = pre.clientWidth;
  let floor = w * minScale;
  if (fit && floor > fit && floor <= fit * 1.06) floor = fit;
  el.style.minWidth = `${Math.floor(floor)}px`;
}

// Show the Expand button and a "scroll" cue on diagrams wider than their figure.
function markScrollable() {
  for (const pre of document.querySelectorAll<HTMLElement>('figure.diagram pre.mermaid')) {
    const fig = pre.closest('figure')!;
    fig.toggleAttribute('data-scrolls', pre.scrollWidth > pre.clientWidth + 2);
  }
}
let resizeTimer = 0;
addEventListener('resize', () => { clearTimeout(resizeTimer); resizeTimer = window.setTimeout(markScrollable, 150); });

function attachExpanders() {
  for (const fig of document.querySelectorAll<HTMLElement>('figure.diagram')) {
    if (fig.querySelector('.diagram__expand')) continue;
    const btn = document.createElement('button');
    btn.className = 'diagram__expand';
    btn.type = 'button';
    btn.textContent = 'Expand ⤢';
    btn.setAttribute('aria-label', 'Expand diagram full screen');
    btn.addEventListener('click', (e) => { e.stopPropagation(); openDialog(fig.querySelector('svg')); });
    fig.append(btn);
    // On touch screens a tap anywhere on the diagram opens it full screen.
    if (matchMedia('(hover: none)').matches) fig.addEventListener('click', () => openDialog(fig.querySelector('svg')));
  }
}

function openDialog(svg: SVGSVGElement | null) {
  if (!svg) return;
  const dlg = document.createElement('dialog');
  dlg.className = 'diagram-dialog';
  dlg.innerHTML = `<div class="stage"></div><button class="diagram__expand close" style="opacity:1">Close ✕</button><div class="hint">Scroll to zoom · drag to pan · Esc to close</div>`;
  const stage = dlg.querySelector('.stage') as HTMLElement;
  const clone = svg.cloneNode(true) as SVGSVGElement;
  const vb = svg.viewBox.baseVal;
  clone.removeAttribute('style');
  clone.setAttribute('width', String(vb?.width || svg.clientWidth));
  clone.setAttribute('height', String(vb?.height || svg.clientHeight));
  stage.append(clone);
  document.body.append(dlg);
  dlg.showModal();
  const fit = Math.min((stage.clientWidth * 0.92) / (vb?.width || 1), (stage.clientHeight * 0.92) / (vb?.height || 1));
  let k = Math.max(0.2, fit), x = 0, y = 0, drag: { x: number; y: number } | null = null;
  const apply = () => (clone.style.transform = `translate(${x}px, ${y}px) scale(${k})`);
  clone.style.transformOrigin = 'center';
  apply();
  stage.addEventListener('wheel', (e) => { e.preventDefault(); k = Math.min(8, Math.max(0.1, k * (e.deltaY < 0 ? 1.12 : 0.89))); apply(); }, { passive: false });
  stage.addEventListener('pointerdown', (e) => { drag = { x: e.clientX - x, y: e.clientY - y }; stage.setPointerCapture(e.pointerId); });
  stage.addEventListener('pointermove', (e) => { if (drag) { x = e.clientX - drag.x; y = e.clientY - drag.y; apply(); } });
  stage.addEventListener('pointerup', () => (drag = null));
  dlg.querySelector('.close')!.addEventListener('click', () => dlg.close());
  dlg.addEventListener('close', () => dlg.remove());
}

// Checklists: GitHub renders task lists as disabled checkboxes. Make them
// tickable and remember the ticks for this page in this browser tab only.
function enableChecklists() {
  const boxes = [...document.querySelectorAll<HTMLInputElement>('.sl-markdown-content .task-list-item > input[type="checkbox"]')];
  if (!boxes.length) return;
  const key = `pp-checklist:${location.pathname}`;
  let saved: boolean[] = [];
  try { saved = JSON.parse(sessionStorage.getItem(key) ?? '[]'); } catch { saved = []; }
  boxes.forEach((box, i) => {
    box.disabled = false;
    box.setAttribute('aria-label', box.parentElement?.textContent?.trim().slice(0, 80) ?? 'Checklist item');
    if (saved[i]) box.checked = true;
    box.addEventListener('change', () => {
      try { sessionStorage.setItem(key, JSON.stringify(boxes.map((b) => b.checked))); } catch { /* storage unavailable */ }
    });
  });
}

enableChecklists();
renderMermaid();
new MutationObserver(() => {
  for (const pre of document.querySelectorAll<HTMLPreElement>('pre.mermaid')) if (pre.dataset.source) pre.textContent = pre.dataset.source;
  renderMermaid();
}).observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
