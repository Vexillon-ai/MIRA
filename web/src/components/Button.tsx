// SPDX-License-Identifier: AGPL-3.0-or-later

// web/src/components/Button.tsx
//
// The canonical MIRA button. Use this instead of a raw <button> — a bare
// <button> falls back to Tailwind's preflight reset and renders as plain text.
// The `no-restricted-syntax` ESLint guard nudges new code here.
//
//   <Button onClick={…}>Save</Button>
//   <Button variant="primary">Add account</Button>
//   <Button variant="ghost" size="sm">Edit</Button>
//   <Button variant="danger" onClick={del}>Delete</Button>

import type { ButtonHTMLAttributes } from 'react'
import styles from './Button.module.css'

type Variant = 'default' | 'primary' | 'ghost' | 'danger'
type Size = 'sm' | 'md'

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant
  size?: Size
}

export default function Button({
  variant = 'default', size = 'md', className, type, ...rest
}: ButtonProps) {
  const cls = [styles.btn, styles[variant], styles[size], className].filter(Boolean).join(' ')
  // Default to type="button" so a button inside a <form> doesn't submit by accident.
  return <button type={type ?? 'button'} className={cls} {...rest} />
}
