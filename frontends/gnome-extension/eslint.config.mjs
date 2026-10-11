// SPDX-License-Identifier: GPL-2.0-or-later
//
// ESLint flat config for the extension (run: `make lint`, needs
// `npm i -D eslint@9 @eslint/js@9 globals`). Shell/GJS globals are declared
// here; `gi://` and `resource://` imports are left unresolved on purpose.
import js from '@eslint/js';
import globals from 'globals';

const gjs = {
    console: 'readonly', print: 'readonly', printerr: 'readonly', log: 'readonly',
    logError: 'readonly', imports: 'readonly', global: 'readonly',
    TextDecoder: 'readonly', TextEncoder: 'readonly',
};

export default [
    {ignores: ['dist/**', 'node_modules/**']},
    js.configs.recommended,
    {
        files: ['opencut@opencut.github.io/**/*.js'],
        languageOptions: {ecmaVersion: 2024, sourceType: 'module', globals: gjs},
        rules: {
            'no-unused-vars': ['error', {argsIgnorePattern: '^_', varsIgnorePattern: '^_'}],
            'eqeqeq': 'error',
            'no-var': 'error',
            'prefer-const': 'error',
        },
    },
    {
        files: ['tests/**/*.mjs'],
        languageOptions: {ecmaVersion: 2024, sourceType: 'module', globals: {...gjs, ...globals.node}},
    },
];
