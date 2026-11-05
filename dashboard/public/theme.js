if (localStorage.getItem('theme') === 'dark' ||
  (!localStorage.getItem('theme') && matchMedia('(prefers-color-scheme: dark)').matches))
  document.documentElement.classList.add('dark')
