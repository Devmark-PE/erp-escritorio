// Sin consola extra en Windows al abrir el programa.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
};

use tauri::{
    ipc::Channel,
    menu::{Menu, MenuItem, Submenu},
    webview::DownloadEvent,
    Manager, State, WebviewWindowBuilder,
};
use tauri_plugin_updater::UpdaterExt;

const RUTA_POR_DEFECTO: &str = r"C:\Users\Jaime Tarazona\Documents\GitHub\Harvey - Asesor de ventas";
const SCRIPT: &str = "reportes/enviar_erp.py";
const ERP_NUBE: &str = "https://admin.devmarkpe.com";

/// Solo una sincronizacion a la vez: dos subidas en paralelo repetirian lotes.
#[derive(Default)]
struct Sincronizando(AtomicBool);

fn archivo_de_ruta(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("harvey-ruta.txt"))
}

fn ruta_guardada(app: &tauri::AppHandle) -> String {
    archivo_de_ruta(app)
        .ok()
        .and_then(|f| fs::read_to_string(f).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| RUTA_POR_DEFECTO.to_string())
}

fn validar(ruta: &str) -> Result<PathBuf, String> {
    let carpeta = PathBuf::from(ruta);
    if carpeta.join(SCRIPT).is_file() {
        Ok(carpeta)
    } else {
        Err(format!("No encuentro {SCRIPT} en \"{ruta}\". Elige la carpeta de Harvey."))
    }
}

/// Python de Harvey: su `.venv` si existe, y si no el del sistema.
fn python(carpeta: &Path) -> PathBuf {
    for relativa in [".venv/Scripts/python.exe", ".venv/bin/python"] {
        let candidato = carpeta.join(relativa);
        if candidato.is_file() {
            return candidato;
        }
    }
    PathBuf::from(if cfg!(windows) { "python" } else { "python3" })
}

/// El unico proceso que este programa lanza: `python reportes/enviar_erp.py <args fijos>`.
fn lanzar(carpeta: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(python(carpeta));
    cmd.current_dir(carpeta)
        .arg(SCRIPT)
        .args(args)
        .env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUNBUFFERED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000); // sin ventana negra
    cmd
}

#[tauri::command]
fn harvey_ruta_leer(app: tauri::AppHandle) -> String {
    ruta_guardada(&app)
}

#[tauri::command]
fn harvey_ruta_guardar(app: tauri::AppHandle, ruta: String) -> Result<String, String> {
    let ruta = ruta.trim().to_string();
    validar(&ruta)?;
    fs::write(archivo_de_ruta(&app)?, &ruta).map_err(|e| e.to_string())?;
    Ok(ruta)
}

/// JSON de `enviar_erp.py --estado`: leads locales, subidos, pendientes y latido. Con `probar`
/// anade `api` (alcanzable y firma aceptada) y es lo unico que usa la red.
#[tauri::command(async)]
fn harvey_estado(app: tauri::AppHandle, probar: Option<bool>) -> Result<String, String> {
    let carpeta = validar(&ruta_guardada(&app))?;
    let args: &[&str] = if probar.unwrap_or(false) { &["--estado", "--probar"] } else { &["--estado"] };
    let salida = lanzar(&carpeta, args)
        .output()
        .map_err(|e| format!("No pude ejecutar Python: {e}. ¿Está instalado y en el PATH?"))?;
    if !salida.status.success() {
        let detalle = String::from_utf8_lossy(&salida.stderr);
        return Err(detalle.lines().last().unwrap_or("falló sin detalle").to_string());
    }
    Ok(String::from_utf8_lossy(&salida.stdout).trim().to_string())
}

/// Sube lo pendiente y reenvia cada linea JSON de Python al canal (inicio, lote, fin, error).
#[tauri::command(async)]
fn harvey_sincronizar(app: tauri::AppHandle, ocupado: State<Sincronizando>, avance: Channel<String>) -> Result<(), String> {
    if ocupado.0.swap(true, Ordering::SeqCst) {
        return Err("Ya hay una sincronización en curso.".into());
    }
    let resultado = sincronizar(&app, &avance);
    ocupado.0.store(false, Ordering::SeqCst);
    resultado
}

fn sincronizar(app: &tauri::AppHandle, avance: &Channel<String>) -> Result<(), String> {
    let carpeta = validar(&ruta_guardada(app))?;
    let mut hijo = lanzar(&carpeta, &["--enviar", "--pendientes", "--json"])
        .spawn()
        .map_err(|e| format!("No pude ejecutar Python: {e}. ¿Está instalado y en el PATH?"))?;
    // stderr en otro hilo para que un buffer lleno no frene a Python; solo importa el final.
    let mut stderr = hijo.stderr.take().ok_or("sin stderr")?;
    let errores = thread::spawn(move || {
        let mut t = String::new();
        let _ = stderr.read_to_string(&mut t);
        t
    });
    let mut emitio_error = false;
    for linea in BufReader::new(hijo.stdout.take().ok_or("sin stdout")?).lines().map_while(Result::ok) {
        emitio_error |= linea.contains(r#""evento": "error""#);
        let _ = avance.send(linea);
    }
    let estado = hijo.wait().map_err(|e| e.to_string())?;
    let stderr = errores.join().unwrap_or_default();
    if !estado.success() && !emitio_error {
        let ultima = stderr.lines().last().unwrap_or("Python terminó con error").to_string();
        let _ = avance.send(serde_json::json!({ "evento": "error", "tipo": "python", "mensaje": ultima }).to_string());
    }
    Ok(())
}

/// Primer nombre libre en la carpeta: `a.pdf`, `a (1).pdf`, `a (2).pdf`... Asi una descarga nunca pisa otra.
fn nombre_libre(dir: &Path, nombre: &str) -> PathBuf {
    let base = Path::new(nombre);
    let tallo = base.file_stem().and_then(|t| t.to_str()).unwrap_or("descarga");
    let ext = base.extension().and_then(|e| e.to_str()).map(|e| format!(".{e}")).unwrap_or_default();
    let mut ruta = dir.join(nombre);
    let mut n = 1;
    while ruta.exists() {
        ruta = dir.join(format!("{tallo} ({n}){ext}"));
        n += 1;
    }
    ruta
}

/// Le dice a la pagina como salio la descarga: `window` recibe `erp-descarga` con `{ ok, detalle }` y el panel lo
/// muestra como aviso flotante (apps/web/src/components/erp/AvisoDescargas.tsx). No usa IPC: el ERP remoto no tiene permisos.
fn avisar_descarga(webview: &tauri::Webview, ok: bool, detalle: &str) {
    let carga = serde_json::json!({ "ok": ok, "detalle": detalle });
    let _ = webview.eval(format!("window.dispatchEvent(new CustomEvent('erp-descarga',{{detail:{carga}}}))"));
}

/// Al abrir busca una version nueva en GitHub Releases (firmada con nuestra clave); si hay, la instala y reinicia.
/// Sin conexion, sin version nueva o con cualquier fallo, el programa sigue como esta: nunca bloquea el arranque.
async fn actualizar(app: tauri::AppHandle) {
    let Ok(updater) = app.updater() else { return };
    let Ok(Some(nueva)) = updater.check().await else { return };
    let ventana = app.get_webview_window("main");
    if let Some(v) = &ventana {
        let _ = v.set_title(&format!("ERP DEVMARK - actualizando a {}...", nueva.version));
    }
    if nueva.download_and_install(|_, _| {}, || {}).await.is_ok() {
        app.restart();
    } else if let Some(v) = &ventana {
        let _ = v.set_title("ERP DEVMARK");
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Sincronizando::default())
        .invoke_handler(tauri::generate_handler![
            harvey_ruta_leer,
            harvey_ruta_guardar,
            harvey_estado,
            harvey_sincronizar
        ])
        .setup(|app| {
            // Una vez dentro del ERP de la nube no hay boton para volver: el menu lo da siempre.
            // La ventana se crea aqui (en tauri.conf.json va con "create": false) para poder colgarle el manejo de
            // descargas: el webview, sin esto, ignora los enlaces con `download`, los blobs y las URL firmadas.
            let destino = Mutex::new(None::<PathBuf>);
            let configuracion = app.config().app.windows.first().ok_or("sin ventana en la configuracion")?.clone();
            let ventana = WebviewWindowBuilder::from_config(app.handle(), &configuracion)?
                .on_download(move |webview, evento| {
                    match evento {
                        DownloadEvent::Requested { destination, .. } => {
                            let nombre = destination.file_name().and_then(|n| n.to_str()).unwrap_or("descarga").to_string();
                            let carpeta = webview.app_handle().path().download_dir();
                            let Ok(carpeta) = carpeta else {
                                avisar_descarga(&webview, false, "No encuentro la carpeta Descargas de este equipo.");
                                return false;
                            };
                            *destination = nombre_libre(&carpeta, &nombre);
                            *destino.lock().unwrap() = Some(destination.clone());
                        }
                        DownloadEvent::Finished { path, success, .. } => {
                            // macOS no entrega la ruta al terminar: se usa la que se fijo al empezar.
                            let ruta = path.or_else(|| destino.lock().unwrap().take());
                            if success {
                                let nombre = ruta.as_ref().and_then(|r| r.file_name()).and_then(|n| n.to_str()).unwrap_or("El archivo");
                                avisar_descarga(&webview, true, &format!("{nombre} se guardó en Descargas."));
                            } else {
                                // El webview no dice por que falla: lo comun es un error del servidor (sin permiso, archivo borrado) o un corte de red.
                                avisar_descarga(&webview, false, "No se pudo descargar el archivo: el servidor no lo entregó o se cortó la conexión. Intenta de nuevo; si sigue igual, avisa al equipo.");
                            }
                        }
                        _ => {}
                    }
                    true
                })
                .build()?;
            let inicio = ventana.url()?;
            let menu = Menu::with_items(
                app,
                &[&Submenu::with_items(
                    app,
                    "Ir a",
                    true,
                    &[
                        &MenuItem::with_id(app, "inicio", "Inicio", true, None::<&str>)?,
                        &MenuItem::with_id(app, "harvey", "Carpeta de Harvey", true, None::<&str>)?,
                        &MenuItem::with_id(app, "erp", "ERP en la nube", true, None::<&str>)?,
                    ],
                )?],
            )?;
            app.set_menu(menu)?;
            tauri::async_runtime::spawn(actualizar(app.handle().clone()));
            app.on_menu_event(move |app, evento| {
                let destino = match evento.id().as_ref() {
                    "inicio" => inicio.join("index.html"),
                    "harvey" => inicio.join("harvey.html"),
                    "erp" => ERP_NUBE.parse(),
                    _ => return,
                };
                if let (Ok(url), Some(ventana)) = (destino, app.get_webview_window("main")) {
                    let _ = ventana.navigate(url);
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("no se pudo iniciar ERP DEVMARK");
}
