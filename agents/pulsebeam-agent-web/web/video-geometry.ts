export interface VideoGeometry {
  readonly visible: boolean;
  readonly height: number;
}

function visibleInTree(element: HTMLElement): boolean {
  if (element.ownerDocument.hidden) return false;
  for (
    let node: HTMLElement | null = element;
    node;
    node = node.parentElement
  ) {
    const style = node.ownerDocument.defaultView?.getComputedStyle(node);
    if (
      node.hidden ||
      style?.display === "none" ||
      style?.visibility === "hidden" ||
      style?.visibility === "collapse" ||
      Number(style?.opacity || "1") === 0
    )
      return false;
  }
  return true;
}

export function observeVideoGeometry(
  element: HTMLVideoElement,
  changed: (geometry: VideoGeometry) => void,
) {
  const viewport = element.ownerDocument.defaultView;
  let intersecting = true;
  let closed = false;
  const sync = () => {
    if (closed) return;
    const rect = element.getBoundingClientRect();
    const visible =
      element.isConnected &&
      intersecting &&
      visibleInTree(element) &&
      rect.width > 0 &&
      rect.height > 0 &&
      (viewport === null ||
        (rect.bottom > 0 &&
          rect.right > 0 &&
          rect.top < viewport.innerHeight &&
          rect.left < viewport.innerWidth));
    changed({
      visible,
      height: visible
        ? Math.min(
            0xffff_ffff,
            Math.ceil(rect.height * (viewport?.devicePixelRatio || 1)),
          )
        : 0,
    });
  };
  let pixelRatioQuery: MediaQueryList | null = null;
  const onPixelRatioChange = () => {
    watchPixelRatio();
    sync();
  };
  const watchPixelRatio = () => {
    pixelRatioQuery?.removeEventListener("change", onPixelRatioChange);
    pixelRatioQuery =
      viewport?.matchMedia?.(
        `(resolution: ${viewport.devicePixelRatio || 1}dppx)`,
      ) ?? null;
    pixelRatioQuery?.addEventListener("change", onPixelRatioChange);
  };
  watchPixelRatio();
  const resize =
    typeof ResizeObserver === "undefined" ? null : new ResizeObserver(sync);
  resize?.observe(element);
  const intersection =
    typeof IntersectionObserver === "undefined"
      ? null
      : new IntersectionObserver(([entry]) => {
          intersecting = entry?.isIntersecting ?? false;
          sync();
        });
  intersection?.observe(element);
  const mutation = new MutationObserver(sync);
  mutation.observe(element.ownerDocument.documentElement, {
    attributes: true,
    attributeFilter: ["style", "class", "hidden"],
    childList: true,
    subtree: true,
  });
  const onResize = () => {
    watchPixelRatio();
    sync();
  };
  viewport?.addEventListener("resize", onResize);
  viewport?.addEventListener("scroll", sync, true);
  element.ownerDocument.addEventListener("visibilitychange", sync);
  sync();
  return {
    close() {
      if (closed) return;
      closed = true;
      resize?.disconnect();
      intersection?.disconnect();
      mutation.disconnect();
      pixelRatioQuery?.removeEventListener("change", onPixelRatioChange);
      viewport?.removeEventListener("resize", onResize);
      viewport?.removeEventListener("scroll", sync, true);
      element.ownerDocument.removeEventListener("visibilitychange", sync);
    },
  };
}
