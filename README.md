# wayhaven

Демон видеообоев для Wayland (Hyprland), встроенный в оболочку end-4 / illogical-impulse (Quickshell `ii`).

## Установка

- **dots-hyprland (Arch)**: ставится инсталлятором как пакет `illogical-impulse-wayhaven` (`./setup install`).
- **Без пакета**: при выборе видеообоев оболочка сама предложит установить wayhaven в `~/.local/bin` —
  готовый бинарник из [Releases](https://github.com/qvap/wayhaven/releases) (проверка sha256), а если он не
  подходит системе — сборка из исходников через cargo.
- **Вручную**: нужны `rust`, `mpv` (libmpv), `wayland`, EGL.

```bash
cargo build --release --locked && install -Dm755 target/release/wayhaven ~/.local/bin/wayhaven
```

Релизы собирает GitHub Actions в контейнере Arch при пуше тега `vX.Y.Z` (версия должна совпадать с `Cargo.toml`).

## Как устроено

```
 Background.qml ──parallax DP-1 0.33 0.5 1.17──▶ $XDG_RUNTIME_DIR/wayhaven.sock
 config.json (inotify) ──wallpaperPath, videoWallpaper.*──▶ wayhaven ◀── Hyprland socket2 (пауза)
                                                              │
        libmpv (NVDEC, zero-copy в GL) ──▶ текстура «cover × zoom» (1 рендер на кадр видео)
                                                              │
                        композитинг окна параллакса на каждый вывод (layer-shell, слой background)
```

- **Декодирование**: libmpv render API, `hwdec=auto-safe` (на GTX 1660 — `nvdec`, без копирования через CPU).
- **Один декод на все мониторы**: mpv рисует кадр один раз в текстуру размера «экран × workspaceZoom», каждый
  монитор просто сэмплирует своё окно из неё. Параллакс-анимация — это только перерисовка одного квада, mpv не трогается.
- **Тайминг**: `wl_surface.frame` + `eglSwapInterval(0)`. Пока композитор не просит кадр (обои перекрыты),
  кадр видео в текстуру не рендерится, а декодер останавливается сам.
- **Пауза** по событиям Hyprland: при полноэкранном окне или (по умолчанию) при любом тайловом окне на воркспейсе.
- **Параллакс**: оболочка шлёт те же доли `usedFractionX/Y`, что использует для статичных обоев, демон анимирует их
  той же кривой (OutCubic, 600 мс) — видео, виджеты и часы двигаются синхронно. Работает и для воркспейсов, и для боковых панелей.
- **Смена видео** — кроссфейд без перезапуска процесса; при смене на картинку `switchwall.sh` завершает демон
  (GL/CUDA-контекст не висит в памяти зря).
- Однопоточный `poll()`-цикл, никаких async-рантаймов; все вызовы ядра mpv из потока рендера асинхронные.

## Интеграция с оболочкой

| Файл | Что изменено |
|---|---|
| `quickshell/ii/modules/ii/background/Background.qml` | мост параллакса: `Socket` → `wayhaven.sock`, автопереподключение |
| `quickshell/ii/modules/common/Config.qml` | секция `background.videoWallpaper` |
| `quickshell/ii/modules/settings/BackgroundConfig.qml` | раздел «Video wallpaper» в настройках |
| `quickshell/ii/scripts/colors/switchwall.sh` | `phonto` + `phonto-handler.sh` заменены на wayhaven |
| `hypr/custom/scripts/__restore_video_wallpaper.sh` | генерируется `switchwall.sh`, запускает демон при старте Hyprland |

Видеообои выбираются как обычно — через селектор обоев оболочки или `switchwall.sh --image video.mp4`.

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
wayhaven set <video> | clear   временно показать видео / вернуться к конфигу
wayhaven pause|resume|toggle
wayhaven parallax <output|*> <fx> <fy> [zoom]
wayhaven status                состояние в JSON (hwdec, пауза, размеры, параллакс)
wayhaven snapshot <file.ppm>   сохранить текущий кадр текстуры (диагностика)
wayhaven reload | quit
```

Лог демона: `$XDG_RUNTIME_DIR/wayhaven.log`. Пересборка и перезапуск: `./install.sh`.
