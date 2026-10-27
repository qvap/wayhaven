# wayhaven

Демон обоев для Wayland (Hyprland): картинки и видео. Встроен в оболочку end-4 / illogical-impulse (Quickshell `ii`), которая сама обои больше не рисует.

## Установка

- **dots-hyprland (Arch)**: ставится инсталлятором как пакет `illogical-impulse-wayhaven` (`./setup install`).
- **Без пакета**: при выборе обоев оболочка сама предложит установить wayhaven в `~/.local/bin` —
  готовый бинарник из [Releases](https://github.com/qvap/wayhaven/releases) (проверка sha256), а если он не
  подходит системе — сборка из исходников через cargo.
- **Вручную**: нужны `rust`, `mpv` (libmpv), `wayland`, EGL.

```bash
cargo build --release --locked && install -Dm755 target/release/wayhaven ~/.local/bin/wayhaven
```

Релизы собирает GitHub Actions в контейнере Arch при пуше тега `vX.Y.Z` (версия должна совпадать с `Cargo.toml`).

## Как устроено

```
 Background.qml ──parallax DP-1 0.33 0.5 1.17, lock 0|1──▶ $XDG_RUNTIME_DIR/wayhaven.sock
 config.json (inotify) ──wallpaperPath, lockWall, lock.blur.*, videoWallpaper.*──▶ wayhaven ◀── Hyprland socket2 (пауза)
                                                              │
   картинка: крейт image (поток) ──▶ текстура с mip-уровнями
   видео: libmpv (NVDEC, zero-copy в GL) ──▶ текстура «cover × zoom» (1 рендер на кадр видео)
                                                              │
                        композитинг окна параллакса на каждый вывод (layer-shell, слой background)
```

- **Картинки**: декодируются крейтом `image` в отдельном потоке (jpeg, png, webp, gif, bmp, tiff), грузятся в
  текстуру с mip-уровнями (чистое уменьшение больших картинок). mpv для них не создаётся.
- **Декодирование видео**: libmpv render API, `hwdec=auto-safe` (на GTX 1660 — `nvdec`, без копирования через CPU).
- **Один декод на все мониторы**: mpv рисует кадр один раз в текстуру размера «экран × workspaceZoom», каждый
  монитор просто сэмплирует своё окно из неё. Параллакс-анимация — это только перерисовка одного квада, mpv не трогается.
- **Тайминг**: `wl_surface.frame` + `eglSwapInterval(0)`. Пока композитор не просит кадр (обои перекрыты),
  кадр видео в текстуру не рендерится, а декодер останавливается сам.
- **Пауза** по событиям Hyprland: при полноэкранном окне или (по умолчанию) при любом тайловом окне на воркспейсе.
- **Параллакс**: оболочка шлёт те же доли `usedFractionX/Y`, что использует для статичных обоев, демон анимирует их
  той же кривой (OutCubic, 600 мс) — видео, виджеты и часы двигаются синхронно. Работает и для воркспейсов, и для боковых панелей.
- **Смена обоев** — кроссфейд между любыми картинками и видео без перезапуска процесса; при переходе на картинку
  mpv выгружается (GL/CUDA-контекст декодера не висит в памяти зря).
- **Экран блокировки**: оболочка шлёт `lock 1/0`. Демон показывает `background.lockWall`, если он задан, плавно
  приближает и размывает обои (`lock.blur.enable/radius/extraZoom`, размытие — выборка mip-уровня с tent-фильтром),
  затем поднимает поверхность на слой overlay, чтобы прозрачная поверхность блокировки показывала обои, а не окна.
- Однопоточный `poll()`-цикл, никаких async-рантаймов; все вызовы ядра mpv из потока рендера асинхронные.

## Интеграция с оболочкой

| Файл | Что изменено |
|---|---|
| `quickshell/ii/modules/ii/background/Background.qml` | обои не рисует; мост параллакса и блокировки: `Socket` → `wayhaven.sock`, автопереподключение |
| `quickshell/ii/modules/common/Config.qml` | секция `background.videoWallpaper` |
| `quickshell/ii/modules/settings/BackgroundConfig.qml` | раздел «Video wallpaper» в настройках |
| `quickshell/ii/scripts/colors/switchwall.sh` | запускает wayhaven для любых обоев (или предлагает установить) |
| `hypr/custom/scripts/__restore_video_wallpaper.sh` | генерируется `switchwall.sh`, запускает демон при старте Hyprland |

Обои выбираются как обычно — через селектор обоев оболочки или `switchwall.sh --image <файл>`.

## Настройки (`~/.config/illogical-impulse/config.json` → `background.videoWallpaper`)

| Ключ | По умолчанию | |
|---|---|---|
| `pauseMode` | `"tiled"` | `never` / `fullscreen` / `tiled` |
| `crossfade` | `true` | кроссфейд при смене видео |
| `audio`, `volume` | `false`, `50` | звук |
| `speed` | `1.0` | скорость воспроизведения |
| `hwdec` | `"auto-safe"` | значение `--hwdec` для mpv |
| `mpvOptions` | `{}` | любые опции mpv, перекрывают встроенные (применяются с перезапуском плеера) |

Всё, кроме `mpvOptions`, применяется на лету. Масштаб/параллакс берутся из `background.parallax`.

## CLI

```
wayhaven daemon [-v]           запустить демон (повторный запуск — no-op)
wayhaven set <file> | clear    временно показать картинку или видео / вернуться к конфигу
wayhaven pause|resume|toggle
wayhaven parallax <output|*> <fx> <fy> [zoom]
wayhaven lock <0|1>            вид экрана блокировки (шлёт оболочка)
wayhaven status                состояние в JSON (hwdec, пауза, размеры, параллакс)
wayhaven snapshot <file.ppm>   сохранить текущую текстуру обоев (диагностика)
wayhaven reload | quit
```

Лог демона: `$XDG_RUNTIME_DIR/wayhaven.log`. Пересборка и перезапуск: `./install.sh`.
