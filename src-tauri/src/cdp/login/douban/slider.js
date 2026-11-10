async (capture) => {
    const visible = element => element?.getClientRects().length
        && getComputedStyle(element).visibility === "visible";
    const widget = document.querySelector("#tCaptchaDyMainWrap");
    const background = visible(widget)
        ? widget.querySelector(".tencent-captcha-dy__verify-bg-img")
        : document.querySelector("#slideBg, .tc-bg-img");
    const candidates = visible(widget)
        ? [...widget.querySelectorAll(".tencent-captcha-dy__fg-item")]
        : [...document.querySelectorAll("#slideBlock, .tc-fg-img, #tcOperation .tc-fg-item:not(.tc-slider-normal)")];
    const handle = visible(widget)
        ? widget.querySelector(".tencent-captcha-dy__slider-block")
        : document.querySelector("#tcaptcha_drag_button, #tcaptcha_drag_thumb, .tcaptcha_drag_button, .tc-drag-thumb, #tcOperation .tc-slider-normal");
    if (!visible(background) || !visible(handle)) return null;

    const bounds = (visible(widget)
        ? widget.querySelector(".tencent-captcha-dy__image-area")
        : background)?.getBoundingClientRect();
    if (!bounds || bounds.width < 120 || bounds.height < 80 || bounds.width > 800 || bounds.height > 600) return null;
    // drag_ele 模板中拼图、滑轨和手柄都使用 tc-fg-item，只有拼图是小尺寸图层。
    const pieces = candidates.filter(element => {
        const rect = element.getBoundingClientRect();
        return visible(element) && element !== handle
            && rect.width >= 10 && rect.height >= 10
            && rect.width < bounds.width / 2 && rect.height < bounds.height / 2;
    });
    if (pieces.length !== 1) return null;
    const [piece] = pieces;
    const pieceBounds = piece.getBoundingClientRect();
    const handleBounds = handle.getBoundingClientRect();

    const handleX = handleBounds.left + handleBounds.width / 2;
    const handleY = handleBounds.top + handleBounds.height / 2;
    if (!handle.contains(document.elementFromPoint(handleX, handleY))) return null;

    function source(element) {
        const style = getComputedStyle(element);
        const cssUrl = style.backgroundImage.match(/^url\("([^"]+)"\)$/)?.[1];
        const raw = element instanceof HTMLImageElement ? (element.currentSrc || element.src) : cssUrl;
        if (!raw) return null;
        const url = new URL(raw, location.href);
        if (url.protocol !== "https:" || !/(^|\.)captcha\.(qq|gtimg|qcloud)\.com$/.test(url.hostname)) return null;
        return { url: url.href, style, rect: element.getBoundingClientRect(), isImage: !cssUrl };
    }
    const bg = source(background);
    const fg = source(piece);
    if (!bg || !fg) return null;
    const geometry = {
        key: [bg.url, fg.url, fg.style.backgroundPosition].join("\n"),
        width: Math.round(bounds.width),
        height: Math.round(bounds.height),
        pieceX: pieceBounds.left - bounds.left,
        pieceY: pieceBounds.top - bounds.top,
        pieceWidth: pieceBounds.width,
        pieceHeight: pieceBounds.height,
        handleX, handleY,
        viewportWidth: innerWidth,
        viewportHeight: innerHeight,
    };
    if (!capture) return geometry;

    function loadImage(url) {
        const image = new Image();
        image.crossOrigin = "anonymous";
        image.src = url;
        return image.decode().then(() => image);
    }
    function render(layer, image, crop) {
        const canvas = document.createElement("canvas");
        canvas.width = Math.round(crop.width);
        canvas.height = Math.round(crop.height);
        const context = canvas.getContext("2d");
        const left = layer.rect.left - crop.left;
        const top = layer.rect.top - crop.top;
        if (layer.isImage) {
            context.drawImage(image, left, top, layer.rect.width, layer.rect.height);
        } else {
            const scaleX = layer.rect.width / parseFloat(layer.style.width);
            const scaleY = layer.rect.height / parseFloat(layer.style.height);
            const size = layer.style.backgroundSize.split(/\s+/);
            const length = (value, full) => value?.endsWith("%")
                ? parseFloat(value) / 100 * full : parseFloat(value);
            let width = length(size[0], layer.rect.width / scaleX) * scaleX;
            let height = length(size[1], layer.rect.height / scaleY) * scaleY;
            if (!Number.isFinite(width)) width = Number.isFinite(height)
                ? height * image.naturalWidth / image.naturalHeight : image.naturalWidth * scaleX;
            if (!Number.isFinite(height)) height = width / scaleX * image.naturalHeight / image.naturalWidth * scaleY;
            const [x, y] = layer.style.backgroundPosition.split(/\s+/);
            const dx = length(x, (layer.rect.width - width) / scaleX) * scaleX;
            const dy = length(y, (layer.rect.height - height) / scaleY) * scaleY;
            if (![dx, dy, width, height].every(Number.isFinite) || width <= 0 || height <= 0) return null;
            context.drawImage(image, left + dx, top + dy, width, height);
        }
        return canvas.toDataURL("image/png");
    }
    let timeout;
    try {
        const images = await Promise.race([
            Promise.all([loadImage(bg.url), loadImage(fg.url)]),
            new Promise(resolve => { timeout = setTimeout(() => resolve(null), 3500); }),
        ]);
        if (!images) return null;
        const backgroundImage = render(bg, images[0], bounds);
        const pieceImage = render(fg, images[1], pieceBounds);
        if (!backgroundImage || !pieceImage) return null;
        return { geometry, backgroundImage, pieceImage };
    } catch {
        return null;
    } finally {
        clearTimeout(timeout);
    }
}
