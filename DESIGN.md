# Design system — Duck Packages

## Verdade do produto

- Produto: ferramenta GNOME para entender e remover pacotes instalados.
- Público: pessoas que usam Linux desktop e não querem recorrer ao terminal.
- Tarefa principal: identificar um aplicativo e removê-lo com impacto explícito.
- Superfície: produto operacional.

## Direção

- Atributos: nativo, calmo, verificável.
- Antiatributos: promocional, ornamental, permissivo com risco.
- Modo: `quiet-product`.
- Assinatura: a prévia de impacto transforma uma remoção opaca numa lista legível de consequências.
- Limite: aparece somente antes de operações destrutivas; não é decoração recorrente.

## Cores e tipografia

Duck Packages herda todos os tokens semânticos do libadwaita (`window_bg_color`,
`card_bg_color`, `accent_color`, `warning_color`, `error_color` e seus textos).
Não há paleta paralela. A interface usa Cantarell/system UI via GTK; títulos, corpo e
dados seguem os estilos `title-1`, `title-3`, `heading`, `body` e `caption` do GNOME.
Tamanhos de pacote usam dígitos tabulares quando o tema oferece suporte.

## Espaço, forma e material

- Unidade base: 4 px; escala prática: 4, 8, 12, 16, 24, 32.
- Conteúdo: máximo de 1040 px, gutters de 12 px em janela estreita e 24 px em ampla.
- Catálogo: Applications e Packages usam o componente `CatalogList` em uma `boxed-list` de no máximo 720 px.
- Raios, bordas e elevação: `boxed-list` e overlays do libadwaita.
- Movimento: somente navegação, abertura de busca e progresso; animações do sistema e
  configuração de redução de movimento são respeitadas automaticamente.

## Componentes e estados

- Catálogo: componente `CatalogList` com `ActionRow`, ícone, título, subtítulo, chevron e ativação por linha; Applications e Packages usam a mesma composição.
- Locks de perfil: painel aparece apenas para symlinks `Singleton*` confirmadamente obsoletos; a limpeza exige confirmação e revalida PID, tipo de entrada e conjunto de arquivos.
- Lista técnica: linhas densas, busca e ordenação; nunca card dentro de card.
- Prévia de impacto: título específico, espaço estimado, pacotes afetados e única ação destrutiva.
- Loading: spinner com explicação objetiva.
- Empty: explica que nenhum aplicativo/pacote corresponde à busca.
- Error/diagnóstico: mantém leitura disponível e explica como restaurar o backend.
- Recuperação: remove somente os três symlinks de lock conhecidos; nunca remove dados do perfil nem segue comandos de shell.
- Processos: lista técnica expansível separa sessões verificadas de correspondências
  possíveis; encerrar e forçar encerramento exigem confirmações independentes.
- Diagnóstico de abertura: causa provável e próximo passo aparecem antes do log
  técnico; o log permanece como evidência, não como interpretação principal. A
  classificação é por tipo de falha e nunca por regras vinculadas a aplicativos.
  A ação compacta de cópia oferece o log técnico isolado ou um relatório com
  diagnóstico, próximo passo e log, sem duplicar controles no cabeçalho.
- Success: toast usando o mesmo verbo da ação, “Removido”.

## Voz e acessibilidade

- Registro direto, sem slogans. Verbos: Abrir, Remover, Cancelar, Tentar novamente.
- Erros dizem o que falhou e o próximo passo; confirmação repete o pacote afetado.
- Argumentos de processos não são exibidos; PID, executável e papel bastam para
  verificar a associação sem expor URLs, arquivos ou tokens.
- Todos os controles têm nome acessível, foco visível e alvo mínimo fornecido pelo GTK.
- A ordem de teclado acompanha a ordem visual e nenhum estado depende apenas de cor.

## Anti-padrões locais

- Não simular uma loja: sem avaliações, banners, screenshots ou recomendações.
- Não esconder dependências removidas, não executar shell e não oferecer remoção forçada.
- Não customizar cor destrutiva, chrome de janela ou tipografia do sistema.
- Não mostrar aliases `NoDisplay` nem duplicar um mesmo lançador.

## Verificação

- Viewports: 980×720 e 360×720, temas claro, escuro e alto contraste.
- Gate mínimo Duck Design: 12/14, nenhum eixo zerado.
- Verificar lista de aplicativos, lista de pacotes, busca, detalhe, diagnóstico, prévia, progresso, erro e lista vazia.

## Críticas de implementação

### Primeira passagem

A primeira renderização em 980×720 revelou cartões esticados por toda a largura e
altura da linha. A combinação de `FlowBox` homogênea transformava a grade em uma
pilha de painéis altos, pouco eficiente para comparar aplicativos. A superfície foi
recomposta como `ListBox`/`ActionRow`, usando a mesma composição nativa da lista de
processos: ícone, nome, resumo, chevron e ativação por teclado.

### Segunda passagem

A instância nativa recompilada mantém o header, a busca e as duas listas no mesmo eixo,
preserva linhas utilizáveis na largura mínima e deixa o GTK cuidar de foco,
hover e ativação por teclado. Não há decoração paralela ao libadwaita; a prévia de impacto
continua sendo o único gesto visual específico. Loading, vazio, diagnóstico,
remoção desabilitada, progresso e sucesso possuem estados distintos sem depender
somente de cor.

## Gate Duck Design

- Especificidade: 2/2 — catálogo instalado e impacto de remoção são próprios do produto.
- Coerência do sistema: 2/2 — tokens e componentes vêm do libadwaita.
- Hierarquia: 2/2 — alternância, busca, catálogo e detalhe têm precedência imediata.
- Contenção: 2/2 — não há avaliações, banners promocionais ou controles fora do escopo.
- Craft: 2/2 — alinhamento, truncamento, foco e estados foram refinados após renderizar.
- Responsividade: 1/2 — recompõe por largura; a validação visual estreita permanece manual.
- Acessibilidade e estados: 1/2 — teclado e nomes acessíveis foram implementados; falta auditoria AT-SPI completa.

Total: **12/14**, sem eixo zerado. O modo é `quiet-product`, portanto movimento não
é um eixo de pontuação; as transições nativas respeitam a preferência do sistema.
