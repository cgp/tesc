// Small, prefix-only source hints. This is intentionally not an XML parser: the
// server remains the authority, and a file may be large or deliberately malformed.

const XML_PROLOG = String.raw`(?:<\?xml\b[^?]*\?>\s*)?`;
const STYLESHEET = String.raw`<\?xml-stylesheet\b[^?]*\?>`;

/** Detect WADL from the two conventional markers at the beginning of its file. */
export function detectSource(content, fallback = null) {
  const prefix = String(content ?? "").slice(0, 4096);
  const prolog = prefix.match(new RegExp(`^\\s*${XML_PROLOG}${STYLESHEET}`, "i"));
  if (prolog?.[0].toLowerCase().includes("wadl")) return "wadl";

  const application = prefix.match(
    new RegExp(`^\\s*${XML_PROLOG}(?:${STYLESHEET}\\s*)?<application\\b[^>]*>`, "i")
  );
  if (application && /\bxmlns(?::[-\w.]+)?\s*=\s*["'][^"']*wadl[^"']*["']/i.test(application[0])) {
    return "wadl";
  }
  return fallback;
}
