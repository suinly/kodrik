// Prepare native tab-specific panels ahead of the click: awaiting setup in
// onClicked loses the user gesture in Helium. Enabling does not open a panel.
chrome.sidePanel.setPanelBehavior({openPanelOnActionClick:false}).catch(console.error);
chrome.sidePanel.setOptions({enabled:false}).catch(console.error);
function prepare(tab) {
  if (!Number.isInteger(tab.id)) return;
  return chrome.sidePanel.setOptions({tabId:tab.id,path:`sidepanel.html?tab=${tab.id}`,enabled:true}).catch(console.error);
}
chrome.tabs.query({}).then(tabs=>Promise.all(tabs.map(prepare))).catch(console.error);
chrome.tabs.onCreated.addListener(prepare);
chrome.action.onClicked.addListener(tab => {
  if (Number.isInteger(tab.id)) return chrome.sidePanel.open({tabId:tab.id}).catch(console.error);
});
chrome.tabs.onRemoved.addListener(tabId=>chrome.storage.session.remove(`tab-chat:${tabId}`).catch(console.error));
