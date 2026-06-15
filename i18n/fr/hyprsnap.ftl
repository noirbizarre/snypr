# HyprSnap — Français (fr).
#
# Convention typographique : espace insécable U+00A0 devant `:`, `?`, `!`, `;`
# et entre une valeur et son unité ("3 s"), conformément aux usages français.

## Barre d'outils — outils (info-bulles)
toolbar-tool-select = Sélection
toolbar-tool-rect = Rectangle
toolbar-tool-ellipse = Ellipse
toolbar-tool-arrow = Flèche
toolbar-tool-line = Ligne
toolbar-tool-highlight = Surligneur
toolbar-tool-freehand = Main levée
toolbar-tool-number = Numéro
toolbar-tool-text = Texte
toolbar-tool-blur = Flou
toolbar-tool-redact = Masquer
toolbar-tool-crop = Recadrer

## Barre d'outils — modes (info-bulles)
toolbar-mode-full = Bureau entier
toolbar-mode-screen = Écran
toolbar-mode-window = Fenêtre
toolbar-mode-region = Région

## Barre d'outils — actions / sélecteurs
toolbar-annotate-tooltip = Annoter (Maj-clic ou Maj+Entrée)
toolbar-capture-tooltip-shift = Capturer (Entrée) — Maj pour annoter
toolbar-capture-tooltip-plain = Capturer (Entrée)
toolbar-color-tooltip = Couleur de l'outil (alpha inclus)
toolbar-stroke-solid = Trait plein
toolbar-stroke-dashed = Trait tireté
toolbar-stroke-dotted = Trait pointillé
toolbar-font-size-tooltip = Taille de police (pt)
toolbar-undo-tooltip = Annuler (Ctrl+Z)
toolbar-clear-tooltip = Effacer (Ctrl+L)
toolbar-cursor-tooltip = Inclure le curseur dans la capture
toolbar-delay-tooltip = Délai avant capture, en secondes
toolbar-passthrough-tooltip = Basculer le passe-clic (P)
toolbar-save-tooltip = Enregistrer (Ctrl+S ou Entrée)
toolbar-delay-label = { $secs } s
toolbar-font-size-label = { $pt } pt
toolbar-color-dialog-title = Choisir une couleur

## Aides du sélecteur
selector-hint-region-empty = Glissez pour sélectionner une région — Entrée pour valider, Échap pour annuler
selector-hint-region-size = { $width } × { $height } — Entrée pour valider, Échap pour annuler
selector-hint-full = Bureau entier — Entrée pour valider, Échap pour annuler
selector-hint-screen-selected = Écran sélectionné — Entrée pour valider, Échap pour annuler
selector-hint-screen-pick = Cliquez sur un écran — Entrée pour valider, Échap pour annuler
selector-hint-window-class-title = { $class } : { $title } — Entrée pour valider, Échap pour annuler
selector-hint-window-class = { $class } — Entrée pour valider, Échap pour annuler
selector-hint-window-title = { $title } — Entrée pour valider, Échap pour annuler
selector-hint-window-selected = Fenêtre sélectionnée — Entrée pour valider, Échap pour annuler
selector-hint-window-pick = Cliquez sur une fenêtre — Entrée pour valider, Échap pour annuler

## Menu de la zone de notification
tray-screenshot-full = Capture d'écran (complète)
tray-annotate-region = Annoter une région…
tray-draw-on-screen = Dessiner à l'écran
tray-quit = Quitter

## Notifications de bureau
notify-copied = Capture copiée dans le presse-papiers
notify-saved-single = Capture enregistrée
notify-saved-multi = Captures enregistrées
notify-saved-multi-body = { $first } ({ $count ->
        [one] { $count } fichier
       *[other] { $count } fichiers
    })

## Erreurs visibles par l'utilisateur
error-edit-incompatible-per-output = `--edit` est incompatible avec `--per-output` (l'éditeur d'annotation ne traite qu'une seule image)
error-edit-requires-ui-feature = `--edit` requiert la fonctionnalité `ui` de Cargo ; recompilez avec ou retirez l'option
error-interactive-requires-ui-feature = le sélecteur interactif requiert la fonctionnalité `ui` de Cargo ; passez --region, --full ou une autre option concrète
error-draw-requires-ui-feature = hyprsnap a été compilé sans la fonctionnalité `ui` ; `draw` n'est pas disponible
error-invalid-region = région invalide : { $spec } (attendu X,Y,LxH)
error-invalid-region-size = taille de région invalide : { $size } (attendu LxH)
error-overlay-no-monitor = aucun moniteur n'intersecte la zone d'édition demandée ; rien à annoter
error-daemon-no-response = le démon a fermé la connexion sans répondre
error-daemon-message = démon : { $message }
