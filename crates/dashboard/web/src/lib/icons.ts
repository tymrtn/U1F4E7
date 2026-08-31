// Line-icon catalog: a vendored subset of Lucide (lucide.dev, ISC license),
// each icon flattened to plain path data on the 24×24 grid. Vendoring keeps
// the dashboard self-contained (no icon CDN, no icon-font) and keeps the set
// deliberately small — an icon enters this file only when a surface needs it.
export const ICON_PATHS = {
  'list-checks': ['m3 17 2 2 4-4', 'm3 7 2 2 4-4', 'M13 6h8', 'M13 12h8', 'M13 18h8'],
  inbox: [
    'M22 12h-6l-2 3h-4l-2-3H2',
    'M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z'
  ],
  zap: ['M13 2 3 14h9l-1 8 10-12h-9l1-8z'],
  hourglass: [
    'M5 22h14',
    'M5 2h14',
    'M17 22v-4.172a2 2 0 0 0-.586-1.414L12 12l-4.414 4.414A2 2 0 0 0 7 17.828V22',
    'M7 2v4.172a2 2 0 0 0 .586 1.414L12 12l4.414-4.414A2 2 0 0 0 17 6.172V2'
  ],
  clock: ['M12 2a10 10 0 1 0 0 20 10 10 0 1 0 0-20z', 'M12 6v6l4 2'],
  calendar: [
    'M5 4h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2z',
    'M16 2v4',
    'M8 2v4',
    'M3 10h18'
  ],
  archive: [
    'M3 3h18a1 1 0 0 1 1 1v3a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1z',
    'M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8',
    'M10 12h4'
  ],
  'pen-line': ['M12 20h9', 'M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z'],
  send: ['m22 2-7 20-4-9-9-4Z', 'M22 2 11 13'],
  bot: [
    'M12 8V4H8',
    'M6 8h12a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2v-8a2 2 0 0 1 2-2z',
    'M2 14h2',
    'M20 14h2',
    'M15 13v2',
    'M9 13v2'
  ],
  reply: ['M20 18v-2a4 4 0 0 0-4-4H4', 'm9 17-5-5 5-5'],
  'reply-all': ['m12 17-5-5 5-5', 'm18 17-5-5 5-5', 'M22 18v-1a4 4 0 0 0-4-4h-6'],
  forward: ['M15 17l5-5-5-5', 'M4 18v-2a4 4 0 0 1 4-4h12'],
  mail: [
    'M22 7a2 2 0 0 0-2-2H4a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2z',
    'm22 8-9.06 5.7a2 2 0 0 1-2.12 0L2 8'
  ],
  'mail-open': [
    'M21.2 8.4c.5.38.8.97.8 1.6v10a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V10a2 2 0 0 1 .8-1.6l8-6a2 2 0 0 1 2.4 0z',
    'm22 10-8.97 5.7a1.94 1.94 0 0 1-2.06 0L2 10'
  ],
  paperclip: [
    'm21.44 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l8.57-8.57A4 4 0 1 1 18 8.84l-8.59 8.57a2 2 0 0 1-2.83-2.83l8.49-8.48'
  ],
  trash: [
    'M3 6h18',
    'M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2',
    'M10 11v6',
    'M14 11v6'
  ],
  x: ['M18 6 6 18', 'm6 6 12 12'],
  'shield-check': [
    'M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z',
    'm9 12 2 2 4-4'
  ]
} as const;

export type IconName = keyof typeof ICON_PATHS;

export const ICON_NAMES = Object.keys(ICON_PATHS) as IconName[];
