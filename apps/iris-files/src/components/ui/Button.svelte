<script lang="ts">
  /**
   * Button - reusable button component with consistent styling
   * Variants: primary, ghost, danger, success
   */
  import type { Snippet } from 'svelte';

  interface Props {
    variant?: 'primary' | 'ghost' | 'danger' | 'success';
    size?: 'sm' | 'md' | 'lg';
    disabled?: boolean;
    type?: 'button' | 'submit' | 'reset';
    onclick?: (e: MouseEvent) => void;
    class?: string;
    title?: string;
    children: Snippet;
  }

  let {
    variant = 'ghost',
    size = 'md',
    disabled = false,
    type = 'button',
    onclick,
    class: className = '',
    title,
    children,
  }: Props = $props();

  const variantClasses = {
    primary: 'btn-primary',
    ghost: 'btn-ghost',
    danger: 'btn-danger',
    success: 'btn-success',
  };

  const sizeClasses = {
    sm: 'px-2 py-1 text-xs',
    md: 'px-3 py-1.5 text-sm',
    lg: 'px-4 py-2 text-base',
  };
</script>

<button
  {type}
  {disabled}
  {onclick}
  {title}
  class="{variantClasses[variant]} {sizeClasses[size]} {disabled ? 'opacity-50 cursor-not-allowed' : ''} {className}"
>
  {@render children()}
</button>
