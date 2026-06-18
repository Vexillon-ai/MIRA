import js from '@eslint/js'
import globals from 'globals'
import reactHooks from 'eslint-plugin-react-hooks'
import reactRefresh from 'eslint-plugin-react-refresh'
import tseslint from 'typescript-eslint'
import { defineConfig, globalIgnores } from 'eslint/config'

export default defineConfig([
  globalIgnores(['dist']),
  {
    files: ['**/*.{ts,tsx}'],
    extends: [
      js.configs.recommended,
      tseslint.configs.recommended,
      reactHooks.configs.flat.recommended,
      reactRefresh.configs.vite,
    ],
    languageOptions: {
      ecmaVersion: 2020,
      globals: globals.browser,
    },
    rules: {
      // UX guardrail: a <button> must declare styling (a className or a style).
      // A bare <button> falls back to Tailwind's preflight reset and renders as
      // plain text — the recurring papercut. Prefer the shared <Button>
      // component (src/components/Button.tsx); a genuinely intentional bare
      // button (e.g. styled by a descendant CSS rule) can opt out with an
      // explicit `eslint-disable-next-line no-restricted-syntax -- <reason>`.
      'no-restricted-syntax': [
        'error',
        {
          selector:
            'JSXOpeningElement[name.name="button"]:not(:has(JSXAttribute[name.name="className"])):not(:has(JSXAttribute[name.name="style"]))',
          message:
            'Bare <button> has no styling (it falls back to the Tailwind reset and looks like plain text). Use the shared <Button> component, or add a className/style.',
        },
      ],
    },
  },
])
