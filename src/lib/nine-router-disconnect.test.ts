import settingsSource from '../Settings.tsx?raw';
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
});
