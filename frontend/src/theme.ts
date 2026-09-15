// 主题管理：light / dark / system
export type Theme = 'light' | 'dark' | 'system';

export function getStoredTheme(): Theme {
  return (localStorage.getItem('theme') as Theme) || 'system';
}

export function systemTheme(): 'light' | 'dark' {
  return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
}

export function applyTheme(theme: Theme): void {
  if (theme === 'system') {
    localStorage.removeItem('theme');
    document.documentElement.setAttribute('data-theme', systemTheme());
  } else {
    localStorage.setItem('theme', theme);
    document.documentElement.setAttribute('data-theme', theme);
  }
}

export function watchSystemTheme(cb: () => void): void {
  window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', cb);
}
