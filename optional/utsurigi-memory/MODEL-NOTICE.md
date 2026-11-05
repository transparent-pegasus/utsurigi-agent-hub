# Model asset provenance

The four JSON files in `model/multilingual-e5-small/` came with the supplied
package. Their upstream source is
[`intfloat/multilingual-e5-small`, revision `614241f622f53c4eeff9890bdc4f31cfecc418b3`, `onnx/`](https://huggingface.co/intfloat/multilingual-e5-small/tree/614241f622f53c4eeff9890bdc4f31cfecc418b3/onnx).
Config and special-token files match upstream bytes; `tokenizer.json` has the
same parsed JSON with different formatting. These assets are retained unchanged.

| File | SHA256 |
| --- | --- |
| `config.json` | `bbb7c1333fc4b3e27fbc9cd5d2070aabcc1d4dfb99917c3633e772f97545a6b6` |
| `special_tokens_map.json` | `d05497f1da52c5e09554c0cd874037a083e1dc1b9cfd48034d1c717f1afc07a7` |
| `tokenizer.json` | `0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39` |
| `tokenizer_config.json` | `a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b` |

The [upstream model card](https://huggingface.co/intfloat/multilingual-e5-small/blob/614241f622f53c4eeff9890bdc4f31cfecc418b3/README.md)
declares MIT licensing for the model. Follow its terms and attribution when
using these assets or acquiring weights. No project source license was supplied;
this notice assigns none. The archive contains no model weights.
