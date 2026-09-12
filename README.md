# cosmic-comp (fork)

Fork de [pop-os/cosmic-comp](https://github.com/pop-os/cosmic-comp) com o
ramo experimental `scrolling-workspaces`: workspaces como colunas numa faixa
horizontal contínua (inspirado no niri), em vez da troca discreta por
workspace.

## Scrolling workspaces (experimental)

### Como ativar

Em `~/.config/cosmic/com.system76.CosmicComp.Config/v1/` (ou via
cosmic-settings → Workspaces, conforme a versão):

```json
{
  "workspaces": {
    "workspace_layout": "scrolling"
  }
}
```

Reinicie o comp (ou faça login de novo) para valer.

### O que funciona

- **Pan animado**: navegação por atalhos (Super+Setas, Super+1..9), swipe
  discreto e ativação programática deslizam a faixa com animação curta
  (ease-in-out cúbico) até a coluna alvo.
- **Colunas parciais**: durante o pan/gesto as colunas vizinhas ficam
  parcialmente visíveis nas bordas do output, com clip correto.
- **Nova janela à direita**: semântica de janelas estilo niri — uma janela
  nova abre numa coluna imediatamente à direita da ativa (nova coluna se a
  ativa for a última); diálogos, flutuantes e fullscreen seguem a colocação
  padrão.
- **Gesto contínuo (4 dedos)**: o delta X do dedo arrasta a faixa 1:1 em
  tempo real (clamp nos limites, sem rubber-band); ao soltar, a faixa snapa
  para a coluna mais próxima com a animação curta. O workspace ativo só
  muda no snap.
- **Fullscreen**: uma janela em fullscreen esconde as colunas vizinhas;
  sair restaura a faixa.
- **Multi-output**: cada output tem sua própria faixa/offset (estado por
  output); resize de output e hotplug re-clampam o offset para a coluna
  ativa.
- **Overview (Super+F3)**: abre/fecha sem corromper a faixa; fechar volta
  para a coluna ativa.
- **Flutuantes e sticky**: seguem funcionando como nos outros layouts
  (sticky sobre todas as colunas do output).

### Limitações conhecidas

- Sem fling/inércia no gesto: soltar com pouca distância percorrida
  (menos de meia coluna) volta para a coluna atual; a velocidade não
  carrega para colunas adicionais.
- Sem largura de coluna customizada: toda coluna tem a largura do output.
- O wallpaper desliza junto com a faixa (não é fixo por coluna).
- Navegação com wraparound salta entre índices, mas o pan visual sempre
  percorre colunas adjacentes (nunca atravessa a emenda do wrap).
