import { rootNodeFromAnchor } from '@codama/nodes-from-anchor';
import fs from 'fs';

// Convert pump_fun.json
const pumpFunAnchor = JSON.parse(fs.readFileSync('pump_fun.json', 'utf8'));
const pumpFunCodama = rootNodeFromAnchor(pumpFunAnchor);
fs.writeFileSync('pump_fun_codama.json', JSON.stringify(pumpFunCodama, null, 2));
console.log('Converted pump_fun.json -> pump_fun_codama.json');

// Convert pump_amm.json
const pumpAmmAnchor = JSON.parse(fs.readFileSync('pump_amm.json', 'utf8'));
const pumpAmmCodama = rootNodeFromAnchor(pumpAmmAnchor);
fs.writeFileSync('pump_amm_codama.json', JSON.stringify(pumpAmmCodama, null, 2));
console.log('Converted pump_amm.json -> pump_amm_codama.json');
