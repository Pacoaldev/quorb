import settingsSource from '../Settings.tsx?raw';
import widgetSource from '../Widget.tsx?raw';
import libSource from '../../src-tauri/src/lib.rs?raw';
import nineRouterSource from '../../src-tauri/src/providers/nine_router.rs?raw';
import { describe, expect, it } from 'vitest';

// Contrato de la desconexión de 9Router (regresión del punto 2 del feedback):
// «Quitar» debe borrar contraseña **y** cookie de sesión. Antes solo borraba la
// contraseña (`setSecret('nine_router_password', '')`) y la cookie seguía dando
// acceso hasta caducar. Ahora va por el comando `nineRouterLogout`, que limpia ambos.
describe('9Router disconnect', () => {
  it('borra ambos secretos con nineRouterLogout, no solo la contraseña', () => {
    expect(settingsSource).toContain('api.nineRouterLogout()');
    expect(settingsSource).not.toContain("api.setSecret('nine_router_password'");
  });

  // Regresión del punto 4: «Guardar y probar» debe usar la Base URL del borrador,
  // no la persistida (antes el backend leía la guardada y probaba contra la vieja).
  it('prueba el login contra la Base URL del borrador', () => {
    expect(settingsSource).toContain('api.nineRouterLogin(value, baseUrl)');
    expect(settingsSource).toContain('baseUrl={cfg.nineRouter.baseUrl}');
  });

  // Regresión del punto P1 (3er feedback): la prueba contra la URL del borrador no
  // debe mezclar credenciales entre servidores. Si el destino difiere del
  // persistido, se prueba con una sesión temporal aislada y no se guarda nada.
  it('no persiste credenciales cuando el destino del borrador difiere', () => {
    expect(libSource).toContain('same_destination');
    expect(libSource).toContain('probe_with_password');
    expect(nineRouterSource).toContain('pub fn same_destination');
    expect(nineRouterSource).toContain('pub async fn probe_with_password');
    // La prueba aislada no toca los secretos globales.
    expect(nineRouterSource).toContain('/// destino). No lee ni escribe los secretos globales.');
  });

  // Regresión del punto P3 (3er feedback): el flujo «cambios sin guardar» del paso
  // de conexión debe mapear 9Router a su clave de config; sin ella, `sectionDirty`
  // no detectaba el borrador y «Comprobar ahora» usaba el destino persistido.
  it('mapea nine_router a su clave de config para el aviso de cambios sin guardar', () => {
    expect(settingsSource).toContain("nine_router: 'nineRouter'");
  });

  // 4º feedback P1: guardar una Base URL distinta debe invalidar los secretos del
  // destino anterior (si no, el siguiente fetch envía credenciales de A a B).
  it('save_config invalida los secretos al cambiar la Base URL de 9Router', () => {
    expect(libSource).toContain('nine_router_destination_changed');
    expect(libSource).toContain('forget_nine_router_secrets');
  });

  // 4º feedback P2: conectar/desconectar no cambia la config, así que se necesita
  // una señal explícita de refresco (`refresh-now`) para el widget.
  it('login y logout piden refresco explícito del widget', () => {
    expect(libSource).toContain('refresh-now');
    expect(widgetSource).toContain("listen('refresh-now'");
  });
});
