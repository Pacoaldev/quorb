import { describe, expect, it } from 'vitest';
import { findLogo, normalizeLogoKey, subLogoKey } from '../components/ProviderLogo';

// Los nodos personalizados de 9Router se etiquetan con su id técnico en
// `byProvider`; el nombre visible llega en el `name` del informe («9Router · Zhipu»).
// `subLogoKey` deriva la clave de logo del nombre para que salga el logo correcto.
describe('9Router custom node logo keys', () => {
  it('uses the pretty node name when present', () => {
    expect(subLogoKey('nine_router:anthropic-compatible-0c354d0a-4d67-4c4e-a6ff-1360bc9c9344', '9Router · Zhipu')).toBe('zhipu');
    expect(
      subLogoKey('nine_router:openai-compatible-responses-1855f0b7-4442-4046-8368-4d11dda27b96', '9Router · QWEN - Alibaba'),
    ).toBe('qwen-alibaba');
  });

  it('resolves those names to a real logo file through aliases', () => {
    expect(findLogo(subLogoKey('nine_router:anthropic-compatible-x', '9Router · Zhipu'))).toBeDefined();
    expect(findLogo(subLogoKey('nine_router:openai-compatible-responses-x', '9Router · QWEN - Alibaba'))).toBeDefined();
  });

  it('keeps the sanitized sub id for known providers without a custom name', () => {
    // Un sub-proveedor normal (no nodo): el nombre es «9Router · Kiro».
    expect(subLogoKey('nine_router:kiro', '9Router · Kiro')).toBe('kiro');
    expect(subLogoKey('nine_router:minimax-cn', '9Router · minimax-cn')).toBe('minimax-cn');
  });

  it('normalizes names to logo keys', () => {
    expect(normalizeLogoKey('QWEN - Alibaba')).toBe('qwen-alibaba');
    expect(normalizeLogoKey('Zhipu')).toBe('zhipu');
    expect(normalizeLogoKey('  Minimax CN ')).toBe('minimax-cn');
  });
});
