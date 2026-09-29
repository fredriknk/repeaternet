const assert = require('node:assert/strict');
const test = require('node:test');
const installNavigationControls = require('../src/rf_router_planner/web_assets/main_view_navigation.js');

class FakeElement extends EventTarget {
  constructor(dataset = {}) {
    super();
    this.dataset = dataset;
    this.children = [];
    this.parentElement = null;
  }

  append(child) {
    child.parentElement = this;
    this.children.push(child);
  }

  dispatchEvent(event) {
    const accepted = super.dispatchEvent(event);
    if (event.bubbles && this.parentElement) this.parentElement.dispatchEvent(event);
    return accepted;
  }
}

function navigationFixture() {
  const main = new FakeElement({ mobileDestination: 'map' });
  const workspace = new FakeElement();
  const mobileNav = new FakeElement();
  const mainViewNav = new FakeElement();
  const setupButton = new FakeElement({ mobileDestination: 'setup' });
  const mobileMapButton = new FakeElement({ mobileDestination: 'map' });
  const mobileAnalysisButton = new FakeElement({ mobileDestination: 'analysis' });
  const mapTab = new FakeElement({ mainViewTarget: 'view-map' });
  const analysisTab = new FakeElement({ mainViewTarget: 'view-analysis' });
  main.append(workspace);
  workspace.append(mobileNav);
  workspace.append(mainViewNav);
  mobileNav.append(setupButton);
  mobileNav.append(mobileMapButton);
  mobileNav.append(mobileAnalysisButton);
  mainViewNav.append(mapTab);
  mainViewNav.append(analysisTab);

  const document = {
    querySelectorAll(selector) {
      if (selector === '.mobile-destination-nav [data-mobile-destination]') {
        return [setupButton, mobileMapButton, mobileAnalysisButton];
      }
      if (selector === '[data-mobile-destination]') {
        return [main, setupButton, mobileMapButton, mobileAnalysisButton];
      }
      if (selector === '[data-main-view-target]') return [mapTab, analysisTab];
      throw new Error(`Unexpected selector: ${selector}`);
    },
  };
  return { document, analysisTab, mobileMapButton, setupButton };
}

test('clicking Analysis stays on Analysis instead of bubbling into the mobile Map control', () => {
  const { document, analysisTab } = navigationFixture();
  const mainViews = [];
  const mobileDestinations = [];

  installNavigationControls(document, {
    setMainView: view => mainViews.push(view),
    setMobileDestination: destination => mobileDestinations.push(destination),
  });
  analysisTab.dispatchEvent(new Event('click', { bubbles: true }));

  assert.deepEqual(mainViews, ['view-analysis']);
  assert.deepEqual(mobileDestinations, []);
});

test('mobile controls still route Map and Setup to their respective destinations', () => {
  const { document, mobileMapButton, setupButton } = navigationFixture();
  const mainViews = [];
  const mobileDestinations = [];

  installNavigationControls(document, {
    setMainView: view => mainViews.push(view),
    setMobileDestination: destination => mobileDestinations.push(destination),
  });
  mobileMapButton.dispatchEvent(new Event('click', { bubbles: true }));
  setupButton.dispatchEvent(new Event('click', { bubbles: true }));

  assert.deepEqual(mainViews, ['view-map']);
  assert.deepEqual(mobileDestinations, ['setup']);
});
