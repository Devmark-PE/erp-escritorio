// Los comandos propios se declaran aqui para que cada uno pida su permiso en capabilities/default.json.
// Sin esta lista cualquier ventana podria llamarlos.
const COMANDOS: &[&str] = &["harvey_ruta_leer", "harvey_ruta_guardar", "harvey_estado", "harvey_sincronizar"];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(COMANDOS)),
    )
    .expect("fallo al generar los permisos de la aplicacion");
}
