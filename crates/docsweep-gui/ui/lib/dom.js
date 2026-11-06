// Small DOM helpers. Text from mails and PDFs is untrusted, so everything goes
// in as text nodes or properties; this interface never assigns HTML strings.

export function h(tag, props = {}, ...children) {
  const element = document.createElement(tag);
  for (const [key, value] of Object.entries(props ?? {})) {
    if (value === undefined || value === null || value === false) continue;
    if (key === "class") element.className = value;
    else if (key.startsWith("on")) element.addEventListener(key.slice(2).toLowerCase(), value);
    else if (key in element && key !== "list" && key !== "form") element[key] = value;
    else element.setAttribute(key, value === true ? "" : value);
  }
  append(element, children);
  return element;
}

export function append(parent, children) {
  for (const child of children.flat(Infinity)) {
    if (child === undefined || child === null || child === false) continue;
    parent.append(child instanceof Node ? child : String(child));
  }
}

export function replace(parent, ...children) {
  parent.replaceChildren();
  append(parent, children);
}

// Fixed drawings on a 20-pixel grid, one per concept.
const ICONS = {
  scan: "M3 6.5V4.5A1.5 1.5 0 0 1 4.5 3h2M13.5 3h2A1.5 1.5 0 0 1 17 4.5v2M17 13.5v2a1.5 1.5 0 0 1-1.5 1.5h-2M6.5 17h-2A1.5 1.5 0 0 1 3 15.5v-2M6 10h8",
  invoices: "M5.5 2.5h6l3.5 3.5v10a1.5 1.5 0 0 1-1.5 1.5h-8A1.5 1.5 0 0 1 4 16V4a1.5 1.5 0 0 1 1.5-1.5ZM11 2.5V6.5h4M7 10.5h6M7 13.5h4",
  companies: "M3.5 17V5a1.5 1.5 0 0 1 1.5-1.5h5A1.5 1.5 0 0 1 11.5 5v12M11.5 8.5h3.5A1.5 1.5 0 0 1 16.5 10v7M2 17h16M6.5 7h2M6.5 10.5h2M6.5 14h2",
  mailboxes: "M2.5 6A1.5 1.5 0 0 1 4 4.5h12A1.5 1.5 0 0 1 17.5 6v8a1.5 1.5 0 0 1-1.5 1.5H4A1.5 1.5 0 0 1 2.5 14ZM3 5.5l7 5.5 7-5.5",
  history: "M3 10a7 7 0 1 0 2.2-5.1L3 7M3 3v4h4M10 6.5V10l2.5 1.5",
  check: "M4 10.5l4 4 8-9",
  alert: "M10 3l8 14H2ZM10 8.5v3.5M10 14.6v.1",
  flag: "M5 18V3M5 3.5h9.5l-2.5 3.5 2.5 3.5H5",
  arrow: "M3.5 10h13M11.5 5l5 5-5 5",
  trash: "M3.5 5.5h13M8 5.5V3.5h4v2M5.5 5.5l.8 11a1 1 0 0 0 1 1h5.4a1 1 0 0 0 1-1l.8-11",
  plus: "M10 4v12M4 10h12",
  folder: "M2.5 6A1.5 1.5 0 0 1 4 4.5h3.5l2 2.5H16A1.5 1.5 0 0 1 17.5 8.5v6A1.5 1.5 0 0 1 16 16H4a1.5 1.5 0 0 1-1.5-1.5Z",
  link: "M8 12l4-4M9 5.5l1.2-1.2a3.5 3.5 0 0 1 5 5L14 10.5M11 14.5l-1.2 1.2a3.5 3.5 0 0 1-5-5L6 9.5",
  question: "M7.5 7.5a2.5 2.5 0 1 1 3.6 2.2c-.8.4-1.1 1-1.1 1.8M10 14.6v.1",
  dot: "M10 10h.01",
  chevron: "M7.5 4.5L13 10l-5.5 5.5",
  settings: "M3 6h7M14 6h3M3 14h3M10 14h7M12 4v4M8 12v4",
  file: "M5.5 2.5h6l3.5 3.5v10a1.5 1.5 0 0 1-1.5 1.5h-8A1.5 1.5 0 0 1 4 16V4a1.5 1.5 0 0 1 1.5-1.5ZM11 2.5V6.5h4",
};

export function icon(name) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 20 20");
  svg.setAttribute("class", "icon");
  svg.setAttribute("aria-hidden", "true");
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("d", ICONS[name]);
  svg.append(path);
  return svg;
}

// The app's mark: three strokes, the middle one short, as if swept aside.
export function mark() {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("class", "mark");
  svg.setAttribute("aria-hidden", "true");
  const box = document.createElementNS("http://www.w3.org/2000/svg", "rect");
  box.setAttribute("width", "24");
  box.setAttribute("height", "24");
  box.setAttribute("rx", "6.5");
  box.setAttribute("class", "mark-box");
  svg.append(box);
  for (const [y, end] of [[7.5, 18], [12, 13.5], [16.5, 18]]) {
    const line = document.createElementNS("http://www.w3.org/2000/svg", "line");
    line.setAttribute("x1", "6.5");
    line.setAttribute("x2", String(end));
    line.setAttribute("y1", String(y));
    line.setAttribute("y2", String(y));
    line.setAttribute("class", "mark-line");
    svg.append(line);
  }
  return svg;
}

export function fileName(path) {
  return path ? path.split(/[\\/]/).pop() : "";
}
