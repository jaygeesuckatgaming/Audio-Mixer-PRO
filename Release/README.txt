UhbikWrapper 0.3.2 (patched build)
==================================

Upstream: https://github.com/dlarseninclusive/UhbikWrapper
Fork:     https://github.com/jaygeesuckatgaming/UhbikWrapper
          branch feature/rescan-plugins
License:  MIT (see upstream repository)

Changes in this build
---------------------
- Rescan button in the header (next to View) and View > Rescan Plug-ins, so
  VST3 effects installed after the wrapper was loaded appear immediately
  instead of needing a host restart.
- The scanned plug-in list is cached in
  %APPDATA%\UhbikWrapper\plugin-list.xml. Loading the wrapper restores that
  list instead of scanning every VST3 folder inside its constructor, which
  also removes the multi-second stall when a host restores a scene.
- Builds against current JUCE (createEditorAndMakeActive).

Install
-------
Extract UhbikWrapper.vst3 and copy the whole bundle into
C:\Program Files\Common Files\VST3\, then start the audio mixer once.
