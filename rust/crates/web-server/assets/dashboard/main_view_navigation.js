(function attachNavigationControls(root, createInstaller) {
  const installNavigationControls = createInstaller();
  root.installNavigationControls = installNavigationControls;
  if (typeof module === 'object' && module.exports) module.exports = installNavigationControls;
})(typeof globalThis === 'undefined' ? this : globalThis, function createInstaller() {
  return function installNavigationControls(document, { setMainView, setMobileDestination }) {
    for (const button of document.querySelectorAll('.mobile-destination-nav [data-mobile-destination]')) {
      button.addEventListener('click', () => {
        const destination = button.dataset.mobileDestination;
        if (destination === 'setup') setMobileDestination('setup');
        else setMainView(destination === 'analysis' ? 'view-analysis' : 'view-map');
      });
    }
    for (const tab of document.querySelectorAll('[data-main-view-target]')) {
      tab.addEventListener('click', () => setMainView(tab.dataset.mainViewTarget));
    }
  };
});
