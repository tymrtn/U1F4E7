// SPA mode — no SSR, no prerender. The axum dashboard serves the shell for this
// path and the client router takes over.
export const ssr = false;
export const prerender = false;
