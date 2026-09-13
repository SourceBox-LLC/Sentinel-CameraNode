/// <reference types="vite/client" />

// Vite's ambient client types. This declares the module shapes Vite
// invents at build time — `*.css` and other asset imports,
// `import.meta.env`, `import.meta.hot` — none of which exist as real
// files for TypeScript to resolve.
//
// `npm create vite` scaffolds this file; this project never had it. That
// went unnoticed under TypeScript 5, which let a side-effect import of a
// non-module slide. TypeScript 7 does not:
//
//   src/main.tsx(15,8): error TS2882: Cannot find module or type
//   declarations for side-effect import of './styles.css'.
//
// So the missing reference was a latent gap in the type setup, not
// something the compiler upgrade introduced.
