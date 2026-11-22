const closedBudgets = new Set();
window.addEventListener('beforeprint', () => {
  for (const budget of document.querySelectorAll('.hop-budget:not([open])')) {
    closedBudgets.add(budget);
    budget.open = true;
  }
});
window.addEventListener('afterprint', () => {
  for (const budget of closedBudgets) budget.open = false;
  closedBudgets.clear();
});
